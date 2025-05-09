//! An implementation of Tor's current relay cell cryptography.
//!
//! These are not very good algorithms; they were the best we could come up with
//! in ~2002.  They are somewhat inefficient, and vulnerable to tagging attacks.
//! They should get replaced within the next several years.  For information on
//! some older proposed alternatives so far, see proposals 261, 295, and 298.
//!
//! I am calling this design `tor1`; it does not have a generally recognized
//! name.

use std::marker::PhantomData;

use crate::{circuit::CircuitBinding, crypto::binding::CIRC_BINDING_LEN, Error, Result};

use cipher::{KeyIvInit, StreamCipher};
use digest::{generic_array::GenericArray, Digest};
use tor_cell::{
    chancell::ChanCmd,
    relaycell::{RelayCellFields, RelayCellFormatTrait},
};
use tor_error::internal;
use typenum::Unsigned;

use super::{
    ClientLayer, CryptInit, InboundClientLayer, InboundRelayLayer, OutboundClientLayer,
    OutboundRelayLayer, RelayCellBody, RelayLayer, SENDME_TAG_LEN,
};

/// A CryptState represents one layer of shared cryptographic state between
/// a relay and a client for a single hop, in a single direction.
///
/// For example, if a client makes a 3-hop circuit, then it will have 6
/// `CryptState`s, one for each relay, for each direction of communication.
///
/// Note that although `CryptState` implements [`OutboundClientLayer`],
/// [`InboundClientLayer`], [`OutboundRelayLayer`], and [`InboundRelayLayer`],
/// instance will only be used for one of these roles.
///
/// It is parameterized on a stream cipher and a digest type: most circuits
/// will use AES-128-CTR and SHA1, but v3 onion services use AES-256-CTR and
/// SHA-3.
pub(crate) struct CryptState<SC: StreamCipher, D: Digest + Clone, RCF: RelayCellFormatTrait> {
    /// Stream cipher for en/decrypting cell bodies.
    ///
    /// This cipher is the one keyed with Kf or Kb in the spec.
    cipher: SC,
    /// Digest for authenticating cells to/from this hop.
    ///
    /// This digest is the one keyed with Df or Db in the spec.
    digest: D,
    /// Most recent digest value generated by this crypto.
    last_digest_val: GenericArray<u8, D::OutputSize>,
    /// The format used for relay cells at this layer.
    relay_cell_format: PhantomData<RCF>,
}

/// A pair of CryptStates shared between a client and a relay, one for the
/// outbound (away from the client) direction, and one for the inbound
/// (towards the client) direction.
pub(crate) struct CryptStatePair<SC: StreamCipher, D: Digest + Clone, RCF: RelayCellFormatTrait> {
    /// State for en/decrypting cells sent away from the client.
    fwd: CryptState<SC, D, RCF>,
    /// State for en/decrypting cells sent towards the client.
    back: CryptState<SC, D, RCF>,
    /// A circuit binding key.
    binding: CircuitBinding,
}

impl<SC: StreamCipher + KeyIvInit, D: Digest + Clone, RCF: RelayCellFormatTrait> CryptInit
    for CryptStatePair<SC, D, RCF>
{
    fn seed_len() -> usize {
        SC::KeySize::to_usize() * 2 + D::OutputSize::to_usize() * 2 + CIRC_BINDING_LEN
    }
    fn initialize(mut seed: &[u8]) -> Result<Self> {
        // This corresponds to the use of the KDF algorithm as described in
        // tor-spec 5.2.2
        if seed.len() != Self::seed_len() {
            return Err(Error::from(internal!(
                "seed length {} was invalid",
                seed.len()
            )));
        }

        // Advances `seed` by `n` bytes, returning the advanced bytes
        let mut take_seed = |n: usize| -> &[u8] {
            let res = &seed[..n];
            seed = &seed[n..];
            res
        };

        let dlen = D::OutputSize::to_usize();
        let keylen = SC::KeySize::to_usize();

        let df = take_seed(dlen);
        let db = take_seed(dlen);
        let kf = take_seed(keylen);
        let kb = take_seed(keylen);
        let binding_key = take_seed(CIRC_BINDING_LEN);

        let fwd = CryptState {
            cipher: SC::new(kf.into(), &Default::default()),
            digest: D::new().chain_update(df),
            last_digest_val: GenericArray::default(),
            relay_cell_format: PhantomData,
        };
        let back = CryptState {
            cipher: SC::new(kb.into(), &Default::default()),
            digest: D::new().chain_update(db),
            last_digest_val: GenericArray::default(),
            relay_cell_format: PhantomData,
        };
        let binding = CircuitBinding::try_from(binding_key)?;

        Ok(CryptStatePair { fwd, back, binding })
    }
}

impl<SC, D, RCF> ClientLayer<CryptState<SC, D, RCF>, CryptState<SC, D, RCF>>
    for CryptStatePair<SC, D, RCF>
where
    SC: StreamCipher,
    D: Digest + Clone,
    RCF: RelayCellFormatTrait,
{
    fn split_client_layer(
        self,
    ) -> (
        CryptState<SC, D, RCF>,
        CryptState<SC, D, RCF>,
        CircuitBinding,
    ) {
        (self.fwd, self.back, self.binding)
    }
}

impl<SC: StreamCipher, D: Digest + Clone, RCF: RelayCellFormatTrait> InboundRelayLayer
    for CryptState<SC, D, RCF>
{
    fn originate(&mut self, cmd: ChanCmd, cell: &mut RelayCellBody) -> &[u8] {
        cell.set_digest::<_, RCF>(&mut self.digest, &mut self.last_digest_val);
        self.encrypt_inbound(cmd, cell);
        &self.last_digest_val[..SENDME_TAG_LEN]
    }
    fn encrypt_inbound(&mut self, _cmd: ChanCmd, cell: &mut RelayCellBody) {
        // This is describe in tor-spec 5.5.3.1, "Relaying Backward at Onion Routers"
        self.cipher.apply_keystream(cell.as_mut());
    }
}
impl<SC: StreamCipher, D: Digest + Clone, RCF: RelayCellFormatTrait> OutboundRelayLayer
    for CryptState<SC, D, RCF>
{
    fn decrypt_outbound(&mut self, _cmd: ChanCmd, cell: &mut RelayCellBody) -> Option<&[u8]> {
        // This is describe in tor-spec 5.5.2.2, "Relaying Forward at Onion Routers"
        self.cipher.apply_keystream(cell.as_mut());
        if cell.is_recognized::<_, RCF>(&mut self.digest, &mut self.last_digest_val) {
            Some(&self.last_digest_val[..SENDME_TAG_LEN])
        } else {
            None
        }
    }
}
impl<SC: StreamCipher, D: Digest + Clone, RCF: RelayCellFormatTrait>
    RelayLayer<CryptState<SC, D, RCF>, CryptState<SC, D, RCF>> for CryptStatePair<SC, D, RCF>
{
    fn split_relay_layer(
        self,
    ) -> (
        CryptState<SC, D, RCF>,
        CryptState<SC, D, RCF>,
        CircuitBinding,
    ) {
        let CryptStatePair { fwd, back, binding } = self;
        (fwd, back, binding)
    }
}
// This impl is used for testing and benchmarks, but nothing else.
#[cfg(any(test, feature = "bench"))]
impl<SC: StreamCipher, D: Digest + Clone, RCF: RelayCellFormatTrait> InboundRelayLayer
    for CryptStatePair<SC, D, RCF>
{
    fn originate(&mut self, cmd: ChanCmd, cell: &mut RelayCellBody) -> &[u8] {
        self.back.originate(cmd, cell)
    }

    fn encrypt_inbound(&mut self, cmd: ChanCmd, cell: &mut RelayCellBody) {
        self.back.encrypt_inbound(cmd, cell);
    }
}
// This impl is used for testing and benchmarks, but nothing else.
#[cfg(any(test, feature = "bench"))]
impl<SC: StreamCipher, D: Digest + Clone, RCF: RelayCellFormatTrait> OutboundRelayLayer
    for CryptStatePair<SC, D, RCF>
{
    fn decrypt_outbound(&mut self, cmd: ChanCmd, cell: &mut RelayCellBody) -> Option<&[u8]> {
        self.fwd.decrypt_outbound(cmd, cell)
    }
}

impl<SC: StreamCipher, D: Digest + Clone, RCF: RelayCellFormatTrait> OutboundClientLayer
    for CryptState<SC, D, RCF>
{
    fn originate_for(&mut self, cmd: ChanCmd, cell: &mut RelayCellBody) -> &[u8] {
        cell.set_digest::<_, RCF>(&mut self.digest, &mut self.last_digest_val);
        self.encrypt_outbound(cmd, cell);
        // Note that we truncate the authentication tag here if we are using
        // a digest with a more-than-20-byte length.
        &self.last_digest_val[..SENDME_TAG_LEN]
    }
    fn encrypt_outbound(&mut self, _cmd: ChanCmd, cell: &mut RelayCellBody) {
        // This is a single iteration of the loop described in tor-spec
        // 5.5.2.1, "routing away from the origin."
        self.cipher.apply_keystream(&mut cell.0[..]);
    }
}

impl<SC: StreamCipher, D: Digest + Clone, RCF: RelayCellFormatTrait> InboundClientLayer
    for CryptState<SC, D, RCF>
{
    fn decrypt_inbound(&mut self, _cmd: ChanCmd, cell: &mut RelayCellBody) -> Option<&[u8]> {
        // This is a single iteration of the loop described in tor-spec
        // 5.5.3, "routing to the origin."
        self.cipher.apply_keystream(&mut cell.0[..]);
        if cell.is_recognized::<_, RCF>(&mut self.digest, &mut self.last_digest_val) {
            Some(&self.last_digest_val[..SENDME_TAG_LEN])
        } else {
            None
        }
    }
}

/// Functions on RelayCellBody that implement the digest/recognized
/// algorithm.
///
/// The current relay crypto protocol uses two wholly inadequate fields to
/// see whether a cell is intended for its current recipient: a two-byte
/// "recognized" field that needs to be all-zero; and a four-byte "digest"
/// field containing a running digest of all cells (for this recipient) to
/// this one, seeded with an initial value (either Df or Db in the spec).
///
/// These operations is described in tor-spec section 6.1 "Relay cells"
//
// TODO: It may be that we should un-parameterize the functions
// that use RCF: given our timeline for deployment of CGO encryption,
// it is likely that we will never actually want to  support `tor1` encryption
// with any other format than RelayCellFormat::V0.
impl RelayCellBody {
    /// Returns the byte slice of the `recognized` field.
    fn recognized<RCF: RelayCellFormatTrait>(&self) -> &[u8] {
        &self.0[RCF::FIELDS::RECOGNIZED_RANGE]
    }
    /// Returns the mut byte slice of the `recognized` field.
    fn recognized_mut<RCF: RelayCellFormatTrait>(&mut self) -> &mut [u8] {
        &mut self.0[RCF::FIELDS::RECOGNIZED_RANGE]
    }
    /// Returns the byte slice of the `digest` field.
    fn digest<RCF: RelayCellFormatTrait>(&self) -> &[u8] {
        &self.0[RCF::FIELDS::DIGEST_RANGE]
    }
    /// Returns the mut byte slice of the `digest` field.
    fn digest_mut<RCF: RelayCellFormatTrait>(&mut self) -> &mut [u8] {
        &mut self.0[RCF::FIELDS::DIGEST_RANGE]
    }
    /// Prepare a cell body by setting its digest and recognized field.
    fn set_digest<D: Digest + Clone, RCF: RelayCellFormatTrait>(
        &mut self,
        d: &mut D,
        used_digest: &mut GenericArray<u8, D::OutputSize>,
    ) {
        self.recognized_mut::<RCF>().fill(0); // Set 'Recognized' to zero
        self.digest_mut::<RCF>().fill(0); // Set Digest to zero

        d.update(&self.0[..]);
        // TODO(nickm) can we avoid this clone?  Probably not.
        *used_digest = d.clone().finalize();
        let used_digest_prefix = &used_digest[0..RCF::FIELDS::DIGEST_RANGE.len()];
        self.digest_mut::<RCF>().copy_from_slice(used_digest_prefix);
    }
    /// Check whether this just-decrypted cell is now an authenticated plaintext.
    ///
    /// This method returns true if the `recognized` field is all zeros, and if the
    /// `digest` field is a digest of the correct material.
    ///
    /// If this method returns false, then either further decryption is required,
    /// or the cell is corrupt.
    // TODO #1336: Further optimize and/or benchmark this.
    fn is_recognized<D: Digest + Clone, RCF: RelayCellFormatTrait>(
        &self,
        d: &mut D,
        rcvd: &mut GenericArray<u8, D::OutputSize>,
    ) -> bool {
        use crate::util::ct;

        // Validate 'Recognized' field
        if !ct::is_zero(self.recognized::<RCF>()) {
            return false;
        }

        // Now also validate the 'Digest' field:

        let mut dtmp = d.clone();
        // Add bytes up to the 'Digest' field
        dtmp.update(&self.0[..RCF::FIELDS::DIGEST_RANGE.start]);
        // Add zeroes where the 'Digest' field is
        dtmp.update(RCF::FIELDS::EMPTY_DIGEST);
        // Add the rest of the bytes
        dtmp.update(&self.0[RCF::FIELDS::DIGEST_RANGE.end..]);
        // Clone the digest before finalize destroys it because we will use
        // it in the future
        let dtmp_clone = dtmp.clone();
        let result = dtmp.finalize();

        if ct::bytes_eq(
            self.digest::<RCF>(),
            &result[0..RCF::FIELDS::DIGEST_RANGE.len()],
        ) {
            // Copy useful things out of this cell (we keep running digest)
            *d = dtmp_clone;
            *rcvd = result;
            return true;
        }

        false
    }
}

/// Benchmark utilities for the `tor1` module.
#[cfg(feature = "bench")]
pub(crate) mod bench_utils {
    use super::*;

    /// Public wrapper around the `RelayCellBody` struct.
    #[repr(transparent)]
    pub struct RelayBody(pub(in crate::crypto) RelayCellBody);

    impl From<[u8; 509]> for RelayBody {
        fn from(body: [u8; 509]) -> Self {
            let body = Box::new(body);
            Self(body.into())
        }
    }

    impl RelayBody {
        /// Public wrapper around the `set_digest` method of the `RelayCellBody` struct.
        pub fn set_digest<D: Digest + Clone, RCF: RelayCellFormatTrait>(
            &mut self,
            d: &mut D,
            used_digest: &mut GenericArray<u8, D::OutputSize>,
        ) {
            self.0.set_digest::<D, RCF>(d, used_digest);
        }

        /// Public wrapper around the `is_recognized` method of the `RelayCellBody` struct.
        pub fn is_recognized<D: Digest + Clone, RCF: RelayCellFormatTrait>(
            &self,
            d: &mut D,
            rcvd: &mut GenericArray<u8, D::OutputSize>,
        ) -> bool {
            self.0.is_recognized::<D, RCF>(d, rcvd)
        }
    }
}

#[cfg(test)]
mod test {
    // @@ begin test lint list maintained by maint/add_warning @@
    #![allow(clippy::bool_assert_comparison)]
    #![allow(clippy::clone_on_copy)]
    #![allow(clippy::dbg_macro)]
    #![allow(clippy::mixed_attributes_style)]
    #![allow(clippy::print_stderr)]
    #![allow(clippy::print_stdout)]
    #![allow(clippy::single_char_pattern)]
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::unchecked_duration_subtraction)]
    #![allow(clippy::useless_vec)]
    #![allow(clippy::needless_pass_by_value)]
    //! <!-- @@ end test lint list maintained by maint/add_warning @@ -->

    use tor_cell::relaycell::RelayCellFormatV0;

    use crate::crypto::cell::{
        test::add_layers, InboundClientCrypt, OutboundClientCrypt, Tor1RelayCrypto,
    };

    use super::*;

    // From tor's test_relaycrypt.c

    #[test]
    fn testvec() {
        use digest::XofReader;
        use digest::{ExtendableOutput, Update};

        // (The ....s at the end here are the KH ca)
        const K1: &[u8; 92] =
            b"    'My public key is in this signed x509 object', said Tom assertively.      (N-PREG-VIRYL)";
        const K2: &[u8; 92] =
            b"'Let's chart the pedal phlanges in the tomb', said Tom cryptographically.  (PELCG-GBR-TENCU)";
        const K3: &[u8; 92] =
            b"     'Segmentation fault bugs don't _just happen_', said Tom seethingly.        (P-GUVAT-YL)";

        const SEED: &[u8;108] = b"'You mean to tell me that there's a version of Sha-3 with no limit on the output length?', said Tom shakily.";
        let cmd = ChanCmd::RELAY;

        // These test vectors were generated from Tor.
        let data: &[(usize, &str)] = &include!("../../../testdata/cell_crypt.rs");

        let mut cc_out = OutboundClientCrypt::new();
        let mut cc_in = InboundClientCrypt::new();
        let pair = Tor1RelayCrypto::<RelayCellFormatV0>::initialize(&K1[..]).unwrap();
        add_layers(&mut cc_out, &mut cc_in, pair);
        let pair = Tor1RelayCrypto::<RelayCellFormatV0>::initialize(&K2[..]).unwrap();
        add_layers(&mut cc_out, &mut cc_in, pair);
        let pair = Tor1RelayCrypto::<RelayCellFormatV0>::initialize(&K3[..]).unwrap();
        add_layers(&mut cc_out, &mut cc_in, pair);

        let mut xof = tor_llcrypto::d::Shake256::default();
        xof.update(&SEED[..]);
        let mut stream = xof.finalize_xof();

        let mut j = 0;
        for cellno in 0..51 {
            let mut body = Box::new([0_u8; 509]);
            body[0] = 2; // command: data.
            body[4] = 1; // streamid: 1.
            body[9] = 1; // length: 498
            body[10] = 242;
            stream.read(&mut body[11..]);

            let mut cell = body.into();
            let _ = cc_out.encrypt(cmd, &mut cell, 2.into());

            if cellno == data[j].0 {
                let expected = hex::decode(data[j].1).unwrap();
                assert_eq!(cell.as_ref(), &expected[..]);
                j += 1;
            }
        }
    }
}

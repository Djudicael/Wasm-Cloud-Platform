use aws_lc_rs::{
    encoding::{AsDer, Pkcs8V1Der},
    rsa::{KeyPair as RsaKeyPair, KeySize, PublicKeyComponents},
    signature::KeyPair as _,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use jsonwebtoken::EncodingKey;
use pkcs8::PrivateKeyInfo;

pub struct GeneratedRsaKey {
    pub encoding_key: EncodingKey,
    pub modulus: String,
    pub exponent: String,
}

pub fn generate_rsa_key() -> GeneratedRsaKey {
    let key_pair = RsaKeyPair::generate(KeySize::Rsa2048).expect("generate test RSA key");
    let pkcs8 = <RsaKeyPair as AsDer<Pkcs8V1Der<'static>>>::as_der(&key_pair)
        .expect("serialize test RSA key");
    let private_key = PrivateKeyInfo::try_from(pkcs8.as_ref())
        .expect("parse generated PKCS#8 key")
        .private_key;
    let public = PublicKeyComponents::<Vec<u8>>::from(key_pair.public_key());

    GeneratedRsaKey {
        encoding_key: EncodingKey::from_rsa_der(private_key),
        modulus: URL_SAFE_NO_PAD.encode(public.n),
        exponent: URL_SAFE_NO_PAD.encode(public.e),
    }
}

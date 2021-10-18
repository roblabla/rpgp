use std::boxed::Box;
use std::str;

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use nom::{
    bytes::streaming::{tag, take},
    combinator::{map, map_opt, map_parser, map_res, rest},
    multi::many0,
    number::streaming::{be_u16, be_u32, be_u8},
    IResult,
};
use num_traits::FromPrimitive;
use smallvec::SmallVec;

use crate::crypto::aead::AeadAlgorithm;
use crate::crypto::hash::HashAlgorithm;
use crate::crypto::public_key::PublicKeyAlgorithm;
use crate::crypto::sym::SymmetricKeyAlgorithm;
use crate::de::Deserialize;
use crate::errors::Result;
use crate::packet::signature::types::*;
use crate::types::{
    mpi, CompressionAlgorithm, KeyId, KeyVersion, Mpi, MpiRef, RevocationKey, RevocationKeyClass,
    Version,
};
use crate::util::{clone_into_array, packet_length, read_string};

impl Deserialize for Signature {
    /// Parses a `Signature` packet from the given slice.
    fn from_slice(packet_version: Version, input: &[u8]) -> Result<Self> {
        let (_, pk) = parse(input, packet_version)?;

        Ok(pk)
    }
}

/// Convert an epoch timestamp to a `DateTime`
fn dt_from_timestamp(ts: u32) -> DateTime<Utc> {
    DateTime::<Utc>::from_utc(NaiveDateTime::from_timestamp(i64::from(ts), 0), Utc)
}

// Parse a signature creation time subpacket
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.4
named!(
    signature_creation_time<Subpacket>,
    map!(
        // 4-octet time field
        be_u32,
        |date| Subpacket::SignatureCreationTime(dt_from_timestamp(date))
    )
);

// Parse an issuer subpacket
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.5
#[rustfmt::skip]
named!(issuer<Subpacket>, map!(
    map_res!(complete!(take!(8)), KeyId::from_slice),
    Subpacket::Issuer
));

// Parse a key expiration time subpacket
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.6
#[rustfmt::skip]
named!(key_expiration<Subpacket>, map!(
    // 4-octet time field
    be_u32,
    |date| Subpacket::KeyExpirationTime(dt_from_timestamp(date))
));

/// Parse a preferred symmetric algorithms subpacket
/// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.7
fn pref_sym_alg(body: &[u8]) -> IResult<&[u8], Subpacket> {
    let list: SmallVec<[SymmetricKeyAlgorithm; 8]> = body
        .iter()
        .map(|v| {
            SymmetricKeyAlgorithm::from_u8(*v)
                .ok_or_else(|| format_err!("Invalid SymmetricKeyAlgorithm"))
        })
        .collect::<Result<_>>()
        .map_err(|_err| {
            nom::Err::Error(nom::error::Error::new(body, nom::error::ErrorKind::MapOpt))
        })?;

    Ok((&b""[..], Subpacket::PreferredSymmetricAlgorithms(list)))
}

/// Parse a preferred hash algorithms subpacket
/// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.8
fn pref_hash_alg(body: &[u8]) -> IResult<&[u8], Subpacket> {
    let list: SmallVec<[HashAlgorithm; 8]> = body
        .iter()
        .map(|v| HashAlgorithm::from_u8(*v).ok_or_else(|| format_err!("Invalid HashAlgorithm")))
        .collect::<Result<_>>()
        .map_err(|_err| {
            nom::Err::Error(nom::error::Error::new(body, nom::error::ErrorKind::MapOpt))
        })?;

    Ok((&b""[..], Subpacket::PreferredHashAlgorithms(list)))
}

/// Parse a preferred compression algorithms subpacket
/// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.9
fn pref_com_alg(body: &[u8]) -> IResult<&[u8], Subpacket> {
    let list: SmallVec<[CompressionAlgorithm; 8]> = body
        .iter()
        .map(|v| {
            CompressionAlgorithm::from_u8(*v)
                .ok_or_else(|| format_err!("Invalid CompressionAlgorithm"))
        })
        .collect::<Result<_>>()
        .map_err(|_err| {
            nom::Err::Error(nom::error::Error::new(body, nom::error::ErrorKind::MapOpt))
        })?;

    Ok((&b""[..], Subpacket::PreferredCompressionAlgorithms(list)))
}

// Parse a signature expiration time subpacket
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.10
#[rustfmt::skip]
named!(signature_expiration_time<Subpacket>, map!(
    // 4-octet time field
    be_u32,
    |date| Subpacket::SignatureExpirationTime(dt_from_timestamp(date))
));

// Parse a exportable certification subpacket.
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.11
named!(
    exportable_certification<Subpacket>,
    map!(complete!(be_u8), |v| Subpacket::ExportableCertification(
        v == 1
    ))
);

// Parse a revocable subpacket
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.12
named!(
    revocable<Subpacket>,
    map!(complete!(be_u8), |v| Subpacket::Revocable(v == 1))
);

// Parse a trust signature subpacket.
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.13
#[rustfmt::skip]
named!(trust_signature<Subpacket>, do_parse!(
       depth: be_u8
    >> value: be_u8
    >> (Subpacket::TrustSignature(depth, value))
));

// Parse a regular expression subpacket.
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.14
#[rustfmt::skip]
named!(regular_expression<Subpacket>, map!(
    map!(rest, read_string), Subpacket::RegularExpression
));

// Parse a revocation key subpacket
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.15
#[rustfmt::skip]
named!(revocation_key<Subpacket>, do_parse!(
             class: map_opt!(be_u8, RevocationKeyClass::from_u8)
    >>   algorithm: map_opt!(be_u8, PublicKeyAlgorithm::from_u8)
    // TODO: V5 Keys have 32 octets here
    >>          fp: take!(20)
    >> (Subpacket::RevocationKey(RevocationKey::new(
        class,
        algorithm,
        fp,
    )))
));

// Parse a notation data subpacket
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.16
#[rustfmt::skip]
named!(notation_data<Subpacket>, do_parse!(
                  // Flags
        readable: map!(be_u8, |v| v == 0x80)
    >>            tag!(&[0, 0, 0])
    >>  name_len: be_u16
    >> value_len: be_u16
    >>      name: map!(take!(name_len), read_string)
    >>     value: map!(take!(value_len), read_string)
    >> (Subpacket::Notation(Notation { readable, name, value }))
));

/// Parse a key server preferences subpacket
/// https://tools.ietf.org/html/rfc4880.html#section-5.2.3.17
fn key_server_prefs(body: &[u8]) -> IResult<&[u8], Subpacket> {
    Ok((
        &b""[..],
        Subpacket::KeyServerPreferences(SmallVec::from_slice(body)),
    ))
}

// Parse a preferred key server subpacket
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.18
#[rustfmt::skip]
named!(preferred_key_server<Subpacket>,do_parse!(
       body: map_res!(rest, str::from_utf8)
    >> ({ Subpacket::PreferredKeyServer(body.to_string()) })
));

// Parse a primary user id subpacket
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.19
named!(
    primary_userid<Subpacket>,
    map!(be_u8, |a| Subpacket::IsPrimary(a == 1))
);

// Parse a policy URI subpacket.
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.20
#[rustfmt::skip]
named!(policy_uri<Subpacket>, map!(
    map!(rest, read_string), Subpacket::PolicyURI
));

/// Parse a key flags subpacket
/// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.21
fn key_flags(body: &[u8]) -> IResult<&[u8], Subpacket> {
    Ok((&b""[..], Subpacket::KeyFlags(SmallVec::from_slice(body))))
}

// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.22
#[rustfmt::skip]
named!(signers_userid<Subpacket>, do_parse!(
       body: map_res!(rest, str::from_utf8)
    >> (Subpacket::SignersUserID(body.to_string())))
);

/// Parse a features subpacket
/// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.24
fn features(body: &[u8]) -> IResult<&[u8], Subpacket> {
    Ok((&b""[..], Subpacket::Features(SmallVec::from_slice(body))))
}

// Parse a revocation reason subpacket
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.23
#[rustfmt::skip]
named!(rev_reason<Subpacket>, do_parse!(
         code: map_opt!(be_u8, RevocationCode::from_u8)
    >> reason: map!(rest, read_string)
    >> (Subpacket::RevocationReason(code, reason))
));

// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.25
#[rustfmt::skip]
named!(sig_target<Subpacket>, do_parse!(
        pub_alg: map_opt!(be_u8, PublicKeyAlgorithm::from_u8)
    >> hash_alg: map_opt!(be_u8, HashAlgorithm::from_u8)
    >>     hash: rest
    >> (Subpacket::SignatureTarget(pub_alg, hash_alg, hash.to_vec()))
));

// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3.26
#[rustfmt::skip]
named!(embedded_sig<Subpacket>, map!(call!(parse, Version::New), |sig| {
    Subpacket::EmbeddedSignature(Box::new(sig))
}));

// Parse an issuer subpacket
// Ref: https://tools.ietf.org/html/draft-ietf-openpgp-rfc4880bis-05#section-5.2.3.28
#[rustfmt::skip]
named!(issuer_fingerprint<Subpacket>, do_parse!(
           version: map_opt!(be_u8, KeyVersion::from_u8)
    >> fingerprint: rest
    >> (Subpacket::IssuerFingerprint(version, SmallVec::from_slice(fingerprint)))
));

/// Parse a preferred aead subpacket
fn pref_aead_alg(body: &[u8]) -> IResult<&[u8], Subpacket> {
    let list: SmallVec<[AeadAlgorithm; 2]> = body
        .iter()
        .map(|v| AeadAlgorithm::from_u8(*v).ok_or_else(|| format_err!("Invalid AeadAlgorithm")))
        .collect::<Result<_>>()
        .map_err(|_err| {
            nom::Err::Error(nom::error::Error::new(body, nom::error::ErrorKind::MapOpt))
        })?;

    Ok((&b""[..], Subpacket::PreferredAeadAlgorithms(list)))
}

fn subpacket(typ: SubpacketType, body: &[u8]) -> IResult<&[u8], Subpacket> {
    use self::SubpacketType::*;
    debug!("parsing subpacket: {:?} {}", typ, hex::encode(body));

    let res = match typ {
        SignatureCreationTime => signature_creation_time(body),
        SignatureExpirationTime => signature_expiration_time(body),
        ExportableCertification => exportable_certification(body),
        TrustSignature => trust_signature(body),
        RegularExpression => regular_expression(body),
        Revocable => revocable(body),
        KeyExpirationTime => key_expiration(body),
        PreferredSymmetricAlgorithms => pref_sym_alg(body),
        RevocationKey => revocation_key(body),
        Issuer => issuer(body),
        Notation => notation_data(body),
        PreferredHashAlgorithms => pref_hash_alg(body),
        PreferredCompressionAlgorithms => pref_com_alg(body),
        KeyServerPreferences => key_server_prefs(body),
        PreferredKeyServer => preferred_key_server(body),
        PrimaryUserId => primary_userid(body),
        PolicyURI => policy_uri(body),
        KeyFlags => key_flags(body),
        SignersUserID => signers_userid(body),
        RevocationReason => rev_reason(body),
        Features => features(body),
        SignatureTarget => sig_target(body),
        EmbeddedSignature => embedded_sig(body),
        IssuerFingerprint => issuer_fingerprint(body),
        PreferredAead => pref_aead_alg(body),
        Experimental(n) => Ok((body, Subpacket::Experimental(n, SmallVec::from_slice(body)))),
        Other(n) => Ok((body, Subpacket::Other(n, body.to_vec()))),
    };

    if res.is_err() {
        warn!("invalid subpacket: {:?} {:?}", typ, res);
    }

    res
}

fn subpackets(input: &[u8]) -> IResult<&[u8], Vec<Subpacket>> {
    let (input, packets) = many0(nom::combinator::complete(|input| {
        // the subpacket length (1, 2, or 5 octets)
        let (input, len) = packet_length(input)?;
        // the subpacket type (1 octet)
        let (input, typ) = map_opt(be_u8, SubpacketType::from_u8)(input)?;
        let (input, p) = map_parser(take(len - 1), |b| subpacket(typ, b))(input)?;
        Ok((input, p))
    }))(input)?;

    Ok((input, packets))
}

fn actual_signature<'a>(
    input: &'a [u8],
    typ: &PublicKeyAlgorithm,
) -> nom::IResult<&'a [u8], Vec<Mpi>> {
    match typ {
        &PublicKeyAlgorithm::RSA | &PublicKeyAlgorithm::RSASign => {
            mpi(input).map(|(rest, v)| (rest, vec![v.to_owned()]))
        }
        &PublicKeyAlgorithm::DSA | &PublicKeyAlgorithm::ECDSA | &PublicKeyAlgorithm::EdDSA => {
            nom::multi::fold_many_m_nc(
                input,
                2,
                2,
                mpi,
                Vec::new(),
                |mut acc: Vec<Mpi>, item: MpiRef<'_>| {
                    acc.push(item.to_owned());
                    acc
                },
            )
        }
        &PublicKeyAlgorithm::Private100
        | &PublicKeyAlgorithm::Private101
        | &PublicKeyAlgorithm::Private102
        | &PublicKeyAlgorithm::Private103
        | &PublicKeyAlgorithm::Private104
        | &PublicKeyAlgorithm::Private105
        | &PublicKeyAlgorithm::Private106
        | &PublicKeyAlgorithm::Private107
        | &PublicKeyAlgorithm::Private108
        | &PublicKeyAlgorithm::Private109
        | &PublicKeyAlgorithm::Private110 => mpi(input).map(|(rest, v)| (rest, vec![v.to_owned()])),
        _ => mpi(input).map(|(rest, v)| (rest, vec![v.to_owned()])),
    }
}

// Parse a v2 or v3 signature packet
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.2
fn v3_parser(
    input: &[u8],
    packet_version: Version,
    version: SignatureVersion,
) -> nom::IResult<&[u8], Signature> {
    // One-octet length of following hashed material. MUST be 5.
    let (input, _) = tag(&[5])(input)?;
    // One-octet signature type.
    let (input, typ) = map_opt(be_u8, SignatureType::from_u8)(input)?;
    // Four-octet creation time.
    let (input, created) = map(be_u32, |v| Utc.timestamp(i64::from(v), 0))(input)?;
    // Eight-octet Key ID of signer.
    let (input, issuer) = map_res(take(8u8), KeyId::from_slice)(input)?;
    // One-octet public-key algorithm.
    let (input, pub_alg) = map_opt(be_u8, PublicKeyAlgorithm::from_u8)(input)?;
    // One-octet hash algorithm.
    let (input, hash_alg) = map_opt(be_u8, HashAlgorithm::from_u8)(input)?;
    // Two-octet field holding left 16 bits of signed hash value.
    let (input, ls_hash) = take(2u8)(input)?;
    // One or more multiprecision integers comprising the signature.
    let (input, sig) = actual_signature(input, &pub_alg)?;

    let mut s = Signature::new(
        packet_version,
        version,
        typ,
        pub_alg,
        hash_alg,
        clone_into_array(ls_hash),
        sig,
        vec![],
        vec![],
    );

    s.config.created = Some(created);
    s.config.issuer = Some(issuer);

    Ok((input, s))
}

// Parse a v4 or v5 signature packet
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2.3
fn v4_parser(
    input: &[u8],
    packet_version: Version,
    version: SignatureVersion,
) -> nom::IResult<&[u8], Signature> {
    // One-octet signature type.
    let (input, typ) = map_opt(be_u8, SignatureType::from_u8)(input)?;
    // One-octet public-key algorithm.
    let (input, pub_alg) = map_opt(be_u8, PublicKeyAlgorithm::from_u8)(input)?;
    // One-octet hash algorithm.
    let (input, hash_alg) = map_opt(be_u8, HashAlgorithm::from_u8)(input)?;
    // Two-octet scalar octet count for following hashed subpacket data.
    let (input, hsub_len) = be_u16(input)?;
    // Hashed subpacket data set (zero or more subpackets).
    let (input, hsub) = map_parser(take(hsub_len), subpackets)(input)?;
    // Two-octet scalar octet count for the following unhashed subpacket data.
    let (input, usub_len) = be_u16(input)?;
    // Unhashed subpacket data set (zero or more subpackets).
    let (input, usub) = map_parser(take(usub_len), subpackets)(input)?;
    // Two-octet field holding the left 16 bits of the signed hash value.
    let (input, ls_hash) = take(2u8)(input)?;
    // One or more multiprecision integers comprising the signature.
    let (input, sig) = actual_signature(input, &pub_alg)?;

    let s = Signature::new(
        packet_version,
        version,
        typ,
        pub_alg,
        hash_alg,
        clone_into_array(ls_hash),
        sig,
        hsub,
        usub,
    );
    Ok((input, s))
}

// Parse a signature packet (Tag 2)
// Ref: https://tools.ietf.org/html/rfc4880.html#section-5.2
pub fn parse(i: &[u8], packet_version: Version) -> IResult<&[u8], Signature> {
    let (i, version) = nom::combinator::map_opt(be_u8, SignatureVersion::from_u8)(i)?;
    let (i, signature) = match version {
        SignatureVersion::V2 => v3_parser(i, packet_version, version)?,
        SignatureVersion::V3 => v3_parser(i, packet_version, version)?,
        SignatureVersion::V4 => v4_parser(i, packet_version, version)?,
        SignatureVersion::V5 => v4_parser(i, packet_version, version)?,
    };
    Ok((i, signature))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subpacket_pref_sym_alg() {
        let input = vec![9, 8, 7, 3, 2];
        let (_, res) = pref_sym_alg(input.as_slice()).unwrap();
        assert_eq!(
            res,
            Subpacket::PreferredSymmetricAlgorithms(
                input
                    .iter()
                    .map(|i| SymmetricKeyAlgorithm::from_u8(*i).unwrap())
                    .collect()
            )
        );
    }
}

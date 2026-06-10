use crate::errors::Error;
use crate::messages::constants;
use base64::engine::general_purpose;
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::Rng;
use sha1::Digest as Digest1;
use sha2::Sha256;
use std::borrow::Cow;

type HmacSha = Hmac<Sha256>;
const MAX_SCRAM_ITERATIONS: i32 = 1_000_000;

pub struct ServerSecret {
    pub iteration: i32,
    pub salt_base64: String,
    pub stored_key: Vec<u8>,
    pub server_key: Vec<u8>,
}

pub struct ServerFirstMessage {
    nonce: String,
    client_first_bare: String,
    pub server_first_bare: String,
}

pub struct ClientFinalMessage {
    channel_binding: Vec<u8>,
    pub nonce: String,
    proof: Vec<u8>,
    client_final_without_proof: String,
}

#[derive(Debug)]
pub struct ClientFirstMessage {
    authcid: String,
    authzid: Option<String>,
    pub nonce: String,
    gs2_flag: char,
    pub client_first_bare: String,
}

/// Parses a part of a SCRAM message, after it has been split on commas.
/// Checks to make sure there's a key, and then verifies its the right key.
/// Returns everything after the first '='.
/// Returns a `ExpectedField` error when one of the above conditions fails.
macro_rules! parse_part {
    ($iter:expr, $field:ident, $key:expr) => {
        if let Some(part) = $iter.next() {
            if part.len() < 2 {
                return Err(Error::ScramClientError(format!("unexpected len")));
            } else if &part.as_bytes()[..2] == $key {
                &part[2..]
            } else {
                return Err(Error::ScramClientError(format!("unexpected field")));
            }
        } else {
            return Err(Error::ScramClientError(format!("unexpected field")));
        }
    };
}

pub fn parse_client_first_message(
    password: Cow<str>,
    // Retained (intentionally unused for now) so a future SCRAM-SHA-256-PLUS
    // implementation can branch on TLS without re-threading the flag from the
    // caller - see the gs2_flag='y' note below (SCRAM channel-binding handling).
    _use_tls: bool,
) -> Result<ClientFirstMessage, Error> {
    let data: &str;
    if let Some(sub_password) = password.get(constants::SCRAM_SHA_256.len() + 4 + 1..password.len())
    {
        data = sub_password;
    } else {
        return Err(Error::ScramClientError("password length".to_string()));
    }

    let gs2_flag: char;
    let mut parts = data.split(',');

    // Channel binding
    if let Some(part) = parts.next() {
        if let Some(cb) = part.chars().next() {
            if cb == 'p' {
                /* Client requires channel binding.  We don't support it. */
                return Err(Error::ScramServerError(
                    "unsupported channel binding".to_string(),
                ));
            }
            /* n - Client does not support channel binding */
            /* y - Client supports channel binding, but we're not doing it today */
            if cb != 'n' && cb != 'y' || part.len() > 1 {
                return Err(Error::ScramServerError(
                    "unsupported channel binding".to_string(),
                ));
            }
            // accept gs2_flag='y' even
            // over TLS. RFC 5802 §6 requires rejecting 'y' ONLY when the
            // server actually advertises a channel-binding (-PLUS) mechanism.
            // pg_doorman's scram_start_challenge offers SCRAM-SHA-256 only (no
            // SCRAM-SHA-256-PLUS) and implements no tls-server-end-point
            // binding, so at the protocol level it does NOT support channel
            // binding and MUST accept 'y'. Default libpq (channel_binding=
            // prefer), psycopg3 and pgjdbc send 'y' over TLS against a
            // SCRAM-SHA-256-only server; the previous use_tls-conditioned
            // rejection broke their authentication. The earlier
            // downgrade-protection rationale is moot here: there is no -PLUS
            // offer for a MITM to strip at this SCRAM layer. (The `_use_tls`
            // parameter is retained for a future -PLUS implementation, where
            // advertising -PLUS WOULD require rejecting 'y'.)
            gs2_flag = cb;
        } else {
            return Err(Error::ScramServerError(
                "unsupported channel binding".to_string(),
            ));
        }
    } else {
        return Err(Error::ScramServerError(
            "unsupported channel binding".to_string(),
        ));
    }

    // Authzid
    let authzid = if let Some(part) = parts.next() {
        if part.is_empty() {
            None
        } else if part.len() < 2 || &part.as_bytes()[..2] != b"a=" {
            return Err(Error::ScramClientError("unexpected authzid".to_string()));
        } else {
            Some(&part[2..])
        }
    } else {
        return Err(Error::ScramClientError("unexpected authzid".to_string()));
    };

    // Authcid
    let authcid = parse_part!(parts, Authcid, b"n=");

    // Nonce
    //
    // pre-auth panic. The previous shape sliced
    // `part.as_bytes()[..2]` BEFORE checking `part.len() >= 2`. A
    // SASLInitialResponse whose SCRAM body ends with a 1-byte field
    // (e.g. `n,,n=u,r`) used to panic during the guard evaluation,
    // aborting the auth task. Per-connection DoS that any
    // unauthenticated client could repeatedly trigger, draining
    // `WORKER_PANIC_COUNT` and flooding stderr/syslog. Mirrors the
    // length check in the authzid branch above.
    let nonce = match parts.next() {
        Some(part) if part.len() >= 2 && &part.as_bytes()[..2] == b"r=" => &part[2..],
        _ => {
            return Err(Error::ScramClientError("unexpected nonce".to_string()));
        }
    };

    let mut c1b_parts = Vec::new();
    for (i, c1b_part) in data.split(',').enumerate() {
        if i == 0 || i == 1 {
            continue;
        }
        c1b_parts.push(c1b_part);
    }

    let mut result = ClientFirstMessage {
        authcid: String::from(authcid),
        authzid: None,
        nonce: String::from(nonce),
        gs2_flag,
        client_first_bare: c1b_parts.join(","),
    };
    if let Some(authzid) = authzid {
        result.authzid = Option::from(String::from(authzid));
    }
    Ok(result)
}

// Parses server secret,
// example: SCRAM-SHA-256
//          $4096: // iterations i32
//          L6Nhfyy6pos5mpvTRXQOTQ== // salt
//          $RMoA1BGLjB/LmVJ2iP5N91E0ri/9siV5E3D5DEvfqXU= // stored key
//          :/aRx7mRpU0txwFSzZ5lcj/u/FHCc503fUfGrF12nGx0= // server key
pub fn parse_server_secret(data: &str) -> Result<ServerSecret, Error> {
    // SCRAM-SHA-256$<iterations>:<salt>$<storedkey>:<serverkey>.
    //   ->
    //      <iterations>:<salt>$<storedkey>:<serverkey>.
    let itr_salt_keys = match data.split_once('$') {
        Some((key, value)) => {
            if key != constants::SCRAM_SHA_256 {
                return Err(Error::ScramServerError(
                    "password secret is not scram".to_string(),
                ));
            }
            value
        }
        _ => {
            return Err(Error::ScramServerError(
                "password secret is not scram".to_string(),
            ));
        }
    };
    // <iterations>:<salt>$<storedkey>:<serverkey>
    //    ->
    //       iterations : <salt>$<storedkey>:<serverkey>.
    let salt_keys: &str;
    let iterations: i32;
    match itr_salt_keys.split_once(':') {
        Some((key, value)) => {
            salt_keys = value;
            match key.parse::<i32>() {
                Ok(n) => {
                    if !(1..=MAX_SCRAM_ITERATIONS).contains(&n) {
                        return Err(Error::ScramServerError(
                            "password secret is not scram".to_string(),
                        ));
                    }
                    iterations = n;
                }
                _ => {
                    return Err(Error::ScramServerError(
                        "password secret is not scram".to_string(),
                    ));
                }
            }
        }
        _ => {
            return Err(Error::ScramServerError(
                "password secret is not scram".to_string(),
            ))
        }
    }
    // <salt>$<storedkey>:<serverkey>.
    //    ->
    //       salt : <storedkey>:<serverkey>.
    let keys: &str;
    let salt: &str;
    match salt_keys.split_once('$') {
        Some((key, value)) => {
            keys = value;
            salt = key;
        }
        _ => {
            return Err(Error::ScramServerError(
                "password secret is not scram".to_string(),
            ));
        }
    }
    // try to decode salt.
    match general_purpose::STANDARD.decode(salt) {
        Ok(_) => {}
        _ => {
            return Err(Error::ScramServerError(
                "password secret is not scram".to_string(),
            ))
        }
    }
    match keys.split_once(':') {
        Some((stored_key_str, server_key_str)) => {
            let mut result = ServerSecret {
                iteration: iterations,
                salt_base64: String::from(salt),
                stored_key: vec![],
                server_key: vec![],
            };
            match general_purpose::STANDARD.decode(stored_key_str) {
                Ok(bytes) => result.stored_key = bytes,
                _ => {
                    return Err(Error::ScramServerError(
                        "password secret is not scram".to_string(),
                    ))
                }
            };
            match general_purpose::STANDARD.decode(server_key_str) {
                Ok(bytes) => result.server_key = bytes,
                _ => {
                    return Err(Error::ScramServerError(
                        "password secret is not scram".to_string(),
                    ))
                }
            };
            Ok(result)
        }
        _ => Err(Error::ScramServerError(
            "password secret is not scram".to_string(),
        )),
    }
}

// return r=nonce,s=salt,i=iteration
//  nonce = client.nonce+server.nonce
//  salt = server.salt
//  iteration = server.iteration
pub fn prepare_server_first_response(
    client_nonce: &str,
    client_first_bare: &str,
    server_salt: &str,
    server_iteration: i32,
) -> ServerFirstMessage {
    let mut rng = rand::rng();
    let key = rng.random::<[u8; 18]>(); //  bytes 18 -> base64 24 (( 4*(18/3) ))
    let nonce = client_nonce.to_owned() + &*general_purpose::STANDARD.encode(key);

    let server_first_bare = format!("r={nonce},s={server_salt},i={server_iteration}");
    ServerFirstMessage {
        nonce,
        client_first_bare: client_first_bare.to_string(),
        server_first_bare,
    }
}

// Parse c=biws,r=BOyfcmcVyYfKDshzppisKFQi;v3%I#&aaEle7p7Tf=PGhp%t,p=UtWlJlm9fN1ojyd4yuCcb6f56txj0GEqYmtTTrXoMEA=
// cbind Vec<8>
// nonce String
// proof Vec<8>
pub fn parse_client_final_message(data: Cow<str>) -> Result<ClientFinalMessage, Error> {
    let channel_binding: &str;
    let nonce_pprof_c2wop: &str;

    match data.split_once(',') {
        Some((key, value)) => {
            nonce_pprof_c2wop = value;
            if let Some((second_key, second_value)) = key.split_once('=') {
                if second_key != "c" {
                    return Err(Error::ScramClientError("key is not c".to_string()));
                }
                channel_binding = second_value;
            } else {
                return Err(Error::ScramClientError(
                    "compare channel binding settings".to_string(),
                ));
            }
        }
        _ => {
            return Err(Error::ScramClientError(
                "compare channel binding settings".to_string(),
            ))
        }
    }

    let nonce: &str;
    if let Some((key, _)) = nonce_pprof_c2wop.split_once(',') {
        match key.split_once('=') {
            Some((second_key, second_value)) => {
                if second_key != "r" {
                    return Err(Error::ScramClientError("parse nonce".to_string()));
                }
                nonce = second_value;
            }
            _ => return Err(Error::ScramClientError("parse nonce".to_string())),
        }
    } else {
        return Err(Error::ScramClientError("parse nonce".to_string()));
    }

    // Extension fields may come between nonce and proof, so we
    // grab the *last* fields as proof.
    let proof: &str;
    match data.rsplit_once(',') {
        Some((_, value)) => {
            if let Some((second_key, second_value)) = value.split_once('=') {
                if second_key != "p" {
                    return Err(Error::ScramClientError("parse proof".to_string()));
                }
                proof = second_value;
            } else {
                return Err(Error::ScramClientError("parse proof".to_string()));
            }
        }
        _ => return Err(Error::ScramClientError("parse proof".to_string())),
    };

    let c2_wop: &str;
    if let Some(sub) = data.get(0..data.rfind(',').unwrap()) {
        c2_wop = sub;
    } else {
        return Err(Error::ScramClientError("parse proof".to_string()));
    }

    let mut result = ClientFinalMessage {
        channel_binding: vec![],
        nonce: String::from(nonce),
        proof: vec![],
        client_final_without_proof: String::from(c2_wop),
    };

    match general_purpose::STANDARD.decode(channel_binding) {
        Ok(cbind_bytes) => result.channel_binding = cbind_bytes,
        _ => {
            return Err(Error::ScramClientError(
                "decode channel binding".to_string(),
            ))
        }
    };
    match general_purpose::STANDARD.decode(proof) {
        Ok(proof_bytes) => result.proof = proof_bytes,
        _ => return Err(Error::ScramClientError("decode channel proof".to_string())),
    };

    Ok(result)
}
pub fn prepare_server_final_message(
    client_first: ClientFirstMessage,
    client_final: ClientFinalMessage,
    server_first: ServerFirstMessage,
    server_secret_server_key: Vec<u8>,
    server_secret_stored_key: Vec<u8>,
) -> Result<(String, Vec<u8>), Error> {
    // checks.
    let mut gs_2_header = client_first.gs2_flag.to_string() + ",,";
    if client_first.authzid.is_some() {
        gs_2_header =
            client_first.gs2_flag.to_string() + "," + client_first.authzid.unwrap().as_str() + ",";
    }
    if String::from_utf8_lossy(&client_final.channel_binding) != gs_2_header {
        return Err(Error::ScramClientError(
            "e=channel-bindings-dont-match".to_string(),
        ));
    }
    if server_first.nonce != client_final.nonce {
        return Err(Error::ScramClientError("e=nonce-mismatch".to_string()));
    }
    // Auth = client-first-without-header + , + server-first + , + client-final-without-proof
    // More concretely, this takes the form:
    // n=username,r=c‑nonce,[extensions,]r=c‑nonce‖s‑nonce,s=salt,i=iteration‑count,[extensions,]
    // c=base64(channel‑flag,[a=authzid],channel‑binding),r=c‑nonce‖s‑nonce[,extensions]
    let auth_msg = server_first.client_first_bare
        + ","
        + &*server_first.server_first_bare
        + ","
        + &*client_final.client_final_without_proof;
    let auth2 = auth_msg.clone();

    // ClientProof = p = ClientKey XOR HMAC(H(ClientKey), Auth):
    //
    let mut mac = HmacSha::new_from_slice(&server_secret_stored_key).unwrap();
    mac.update(auth_msg.as_ref());
    let mac_result_stored_key = mac.finalize();

    // XOR
    let client_key_xor: Vec<_> = client_final
        .proof
        .iter()
        .zip(mac_result_stored_key.into_bytes())
        .map(|(x, y)| x ^ y)
        .collect();

    let mut client_key_hasher = Sha256::new();
    client_key_hasher.update(&*client_key_xor);
    let client_proof = client_key_hasher.finalize();

    // check equal two hmac vectors:
    //      ServerSignature and ClientProof
    if client_proof.is_empty() || client_proof.len() != server_secret_stored_key.len() {
        return Err(Error::ScramClientError("e=mismatch-key-length".to_string()));
    }
    let mut is_not_equal: u8 = 0;
    for (i, c) in server_secret_stored_key.iter().enumerate() {
        is_not_equal |= c ^ client_proof[i];
    }
    if is_not_equal != 0 {
        return Err(Error::ScramClientError("e=password-hash".to_string()));
    };

    let mut hmac_result_server_sign = HmacSha::new_from_slice(&server_secret_server_key).unwrap();
    hmac_result_server_sign.update(auth2.as_ref());
    let mac_result_server_key = hmac_result_server_sign.finalize();
    // ServerSignature = v = HMAC(ServerKey, Auth)
    let result = format!(
        "v={}",
        general_purpose::STANDARD.encode(mac_result_server_key.into_bytes())
    );
    Ok((result, client_key_xor))
}

impl std::fmt::Display for ClientFirstMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{{ authzid: {:?}, authcid: {:?}, client_first_bare: {} }}",
            self.authzid, self.authcid, self.client_first_bare
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn good_parse_server_secret() {
        let result = parse_server_secret(
            "SCRAM-SHA-256$4096:L6Nhfyy6pos5mpvTRXQOTQ==$RMoA1BGLjB/LmVJ2iP5N91E0ri/9siV5E3D5DEvfqXU=:/aRx7mRpU0txwFSzZ5lcj/u/FHCc503fUfGrF12nGx0=").unwrap();
        assert_eq!(4096, result.iteration);
        assert_eq!("L6Nhfyy6pos5mpvTRXQOTQ==", result.salt_base64);
    }
    #[test]
    fn bad_parse_server_secret() {
        assert!(parse_server_secret("SCRAM-SHA-256$4096:").is_err());
    }

    #[test]
    fn parse_server_secret_rejects_bad_iteration_counts() {
        for iterations in ["0", "-1", "1000001"] {
            let secret = format!(
                "SCRAM-SHA-256${iterations}:L6Nhfyy6pos5mpvTRXQOTQ==$RMoA1BGLjB/LmVJ2iP5N91E0ri/9siV5E3D5DEvfqXU=:/aRx7mRpU0txwFSzZ5lcj/u/FHCc503fUfGrF12nGx0="
            );
            assert!(
                parse_server_secret(&secret).is_err(),
                "iterations={iterations} must be rejected"
            );
        }
    }

    /// `parse_client_first_message`
    /// used to panic on a SCRAM body whose last field was a
    /// single byte (e.g. just `"r"` instead of `"r=…"`).
    /// `&part.as_bytes()[..2]` on a 1-byte slice was an
    /// out-of-bounds index that aborted the pre-auth task - an
    /// unauthenticated DoS reachable from any client willing to
    /// keep TCP-reconnecting.
    #[test]
    fn parse_client_first_message_short_nonce_field_returns_err_not_panic() {
        // Header "SCRAM-SHA-256\0\0\0\0 " + body whose nonce
        // field is the single byte "r" - no `=value`.
        let payload = "SCRAM-SHA-256\0\0\0\0 n,,n=u,r";
        let result = parse_client_first_message(Cow::from(payload), false);
        assert!(
            matches!(result, Err(Error::ScramClientError(_))),
            "expected ScramClientError, got {result:?}"
        );

        // Same for the authzid branch already guarded -
        // include it so the gate is locked in.
        let payload = "SCRAM-SHA-256\0\0\0\0 n,a,n=u,r=abc";
        let _ = parse_client_first_message(Cow::from(payload), false);
        // Should NOT panic; either Ok(_) or typed Err.
    }

    #[test]
    fn good_parse_client_first_message() {
        let result = parse_client_first_message(
            Cow::from("SCRAM-SHA-256\0\0\0\0 n,,n=,r=5DAkMQDUZpG/3GcwewTYJZbD"),
            false,
        )
        .unwrap();
        assert_eq!("n=,r=5DAkMQDUZpG/3GcwewTYJZbD", result.client_first_bare);
    }

    /// RFC 5802 §6 - a server that does
    /// NOT advertise a channel-binding (-PLUS) mechanism MUST accept the
    /// `gs2_flag='y'` signal. pg_doorman's scram_start_challenge offers
    /// SCRAM-SHA-256 only, so 'y' must be accepted even over TLS; default
    /// libpq/psycopg3/pgjdbc (channel_binding=prefer) send exactly this over
    /// TLS against a SCRAM-SHA-256-only server.
    #[test]
    fn cbind_y_accepted_when_tls_active() {
        // Valid SCRAM first message body with gs2_flag='y' over TLS.
        let payload = "SCRAM-SHA-256\0\0\0\0 y,,n=u,r=abc";
        let result = parse_client_first_message(Cow::from(payload), true);
        assert!(
            result.is_ok(),
            "gs2_flag='y' must be accepted over TLS (no -PLUS advertised), got {result:?}"
        );
    }

    /// Sanity check: without TLS the `'y'` signal is structurally honest
    /// (no cbind material exists either side) and must still be accepted.
    #[test]
    fn cbind_y_accepted_when_tls_inactive() {
        let payload = "SCRAM-SHA-256\0\0\0\0 y,,n=u,r=abc";
        let result = parse_client_first_message(Cow::from(payload), false);
        assert!(result.is_ok(), "expected Ok over plain TCP, got {result:?}");
    }

    #[test]
    fn good_parse_client_final_message_1() {
        let msg = "c=biws,r=BOyfcmcVyYfKDshzppisKFQi;v3%I#&aaEle7p7Tf=PGhp%t,p=UtWlJlm9fN1ojyd4yuCcb6f56txj0GEqYmtTTrXoMEA=";
        let result = parse_client_final_message(Cow::from(msg)).unwrap();
        assert_eq!(
            "BOyfcmcVyYfKDshzppisKFQi;v3%I#&aaEle7p7Tf=PGhp%t",
            result.nonce
        );
        assert_eq!(
            "c=biws,r=BOyfcmcVyYfKDshzppisKFQi;v3%I#&aaEle7p7Tf=PGhp%t",
            result.client_final_without_proof
        );
    }

    #[test]
    fn good_parse_client_final_message_2() {
        let msg = r#"c=biws,r=hpKJtw+2MfwoAzmpDq9EzI2G+z)*rt(C;OK.=.lCw'"{K6t:,p=6UthzFqF75xC9AkNffYvIfy8Vl5D31tkr3IFqMo2h/I="#;
        let result = parse_client_final_message(Cow::from(msg)).unwrap();
        assert_eq!(
            r#"hpKJtw+2MfwoAzmpDq9EzI2G+z)*rt(C;OK.=.lCw'"{K6t:"#,
            result.nonce
        );
    }

    // #[test]
    // fn full_test() {
    //     let server_secrets = parse_server_secret(
    //         "SCRAM-SHA-256$4096:p2j/1lMdQF6r1dD9I9f7PQ==$H3xt5yh7lwSq9zUPYwHovRu3FyUCCXchG/skydJRa9o=:5xU6Wj/GNg3UnN2uQIx3ezx7uZyzGeM5NrvSJRIxnlw=").unwrap();
    //     let first_client_message_bare_vec: Vec<u8> = vec![0x53, 0x43, 0x52, 0x41, 0x4d, 0x2d, 0x53,
    //                                                       0x48, 0x41, 0x2d, 0x32, 0x35, 0x36, 0x00,
    //                                                       0x00, 0x00, 0x00, 0x20, 0x6e, 0x2c, 0x2c,
    //                                                       0x6e, 0x3d, 0x2c, 0x72, 0x3d, 0x38, 0x41,
    //                                                       0x73, 0x54, 0x50, 0x6a, 0x77, 0x71, 0x32,
    //                                                       0x73, 0x54, 0x4b, 0x39, 0x64, 0x41, 0x57,
    //                                                       0x6e, 0x31, 0x79, 0x41, 0x61, 0x71, 0x35,
    //                                                       0x55];
    //     let first_client = parse_client_first_message(String::from_utf8_lossy(
    //         &*first_client_message_bare_vec)).unwrap();
    //     let first_server = prepare_server_first_response(
    //         &*first_client.nonce, &*first_client.client_first_bare,
    //         &*server_secrets.salt_base64, server_secrets.iteration);
    //     // first server response.
    //     assert_eq!("r=8AsTPjwq2sTK9dAWn1yAaq5U5/NHrq9DyUNyJwlL+JlOqN8P,s=p2j/1lMdQF6r1dD9I9f7PQ==,i=4096", first_server.server_first_bare);
    //     let final_client = parse_client_final_message(Cow::from(
    //         "c=biws,r=8AsTPjwq2sTK9dAWn1yAaq5U5/NHrq9DyUNyJwlL+JlOqN8P,p=qOlPqTflW6nmzc32/84FhGVDt+o7nFYQDjjn4i9w/3s=")).unwrap();
    //     let final_server = prepare_server_final_message(first_client, final_client,  first_server,server_secrets.server_key, server_secrets.stored_key, ).unwrap();
    //     // final server response.
    //     assert_eq!("v=JWKBZ02hgdOX7yVFj+/5xEuvA6ThTpvmCedVsli3bIs=", final_server);
    // }
}

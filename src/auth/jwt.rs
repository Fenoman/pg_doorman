use std::collections::{HashMap, HashSet};
use std::fs;
use std::ops::Add;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jwt::{Header, PKeyWithDigest, RegisteredClaims, SignWithKey, Token, VerifyWithKey};
use once_cell::sync::Lazy;
use openssl::hash::MessageDigest;
use openssl::pkey::{Id, PKey, Public};
use openssl::rsa::Rsa;
use serde_derive::{Deserialize, Serialize};
use tokio::sync::{RwLock, RwLockWriteGuard};

use crate::errors::Error;
use crate::messages::JWT_PUB_KEY_PASSWORD_PREFIX;

#[allow(dead_code)]
static KEYS: Lazy<RwLock<HashMap<String, PKeyWithDigest<Public>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
#[allow(dead_code)]
static PUBLISHED_JWT_KEY_FILENAMES: Lazy<RwLock<HashSet<String>>> =
    Lazy::new(|| RwLock::new(HashSet::new()));

pub(crate) struct StagedJwtPubKeys {
    loaded: HashMap<String, PKeyWithDigest<Public>>,
    current_filenames: HashSet<String>,
}

pub(crate) struct JwtPubKeysWriteGuards {
    keys: RwLockWriteGuard<'static, HashMap<String, PKeyWithDigest<Public>>>,
    published_filenames: RwLockWriteGuard<'static, HashSet<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JwtVerifierConfig {
    pub(crate) key_filename: String,
    pub(crate) issuer: String,
    pub(crate) audience: String,
}

#[derive(Serialize, Deserialize)]
pub struct PreferredUsernameClaims {
    #[serde(flatten)]
    default_claims: RegisteredClaims, // https://tools.ietf.org/html/rfc7519#page-9
    #[serde(rename = "preferred_username")]
    username: String, // additional
}

pub fn new_claims(username: String, duration: Duration) -> PreferredUsernameClaims {
    let mut result = PreferredUsernameClaims {
        default_claims: RegisteredClaims::default(),
        username,
    };
    let time = SystemTime::now()
        .add(duration)
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    result.default_claims.expiration = Some(time);
    result
}

pub fn new_claims_with_scope(
    username: String,
    duration: Duration,
    issuer: String,
    audience: String,
) -> PreferredUsernameClaims {
    let mut claims = new_claims(username, duration);
    claims.default_claims.issuer = Some(issuer);
    claims.default_claims.audience = Some(audience);
    claims
}

pub(crate) fn parse_jwt_pub_key_password(
    password: &str,
) -> Result<Option<JwtVerifierConfig>, Error> {
    let Some(spec) = password.strip_prefix(JWT_PUB_KEY_PASSWORD_PREFIX) else {
        return Ok(None);
    };
    let Some((key_filename, scope)) = spec.split_once('?') else {
        return Err(Error::BadConfig(
            "JWT public-key password must include issuer and audience scope: \
             jwt-pkey-fpath:/path/to/public.pem?iss=<issuer>&aud=<audience>"
                .to_string(),
        ));
    };
    if key_filename.is_empty() {
        return Err(Error::BadConfig(
            "JWT public-key password has an empty key filename".to_string(),
        ));
    }

    let mut issuer = None;
    let mut audience = None;
    for part in scope.split('&') {
        let Some((key, value)) = part.split_once('=') else {
            return Err(Error::BadConfig(
                "JWT public-key password scope must use key=value pairs".to_string(),
            ));
        };
        if value.is_empty() {
            return Err(Error::BadConfig(format!(
                "JWT public-key password scope field '{key}' must not be empty"
            )));
        }
        match key {
            "iss" => issuer = Some(value.to_string()),
            "aud" => audience = Some(value.to_string()),
            _ => {
                return Err(Error::BadConfig(format!(
                    "JWT public-key password has unsupported scope field '{key}'"
                )))
            }
        }
    }

    let Some(issuer) = issuer else {
        return Err(Error::BadConfig(
            "JWT public-key password must include issuer scope field 'iss'".to_string(),
        ));
    };
    let Some(audience) = audience else {
        return Err(Error::BadConfig(
            "JWT public-key password must include audience scope field 'aud'".to_string(),
        ));
    };

    Ok(Some(JwtVerifierConfig {
        key_filename: key_filename.to_string(),
        issuer,
        audience,
    }))
}

impl PreferredUsernameClaims {
    fn validate(&self, expected_issuer: &str, expected_audience: &str) -> Result<(), Error> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        if let Some(val) = self.default_claims.not_before {
            if now < val {
                return Err(Error::JWTValidate("not before".to_string()));
            }
        }
        if let Some(val) = self.default_claims.expiration {
            if now > val {
                return Err(Error::JWTValidate("expiration".to_string()));
            }
        } else {
            return Err(Error::JWTValidate("empty expiration".to_string()));
        }
        if self.default_claims.issuer.as_deref() != Some(expected_issuer) {
            return Err(Error::JWTValidate("issuer mismatch".to_string()));
        }
        if self.default_claims.audience.as_deref() != Some(expected_audience) {
            return Err(Error::JWTValidate("audience mismatch".to_string()));
        }
        Ok(())
    }
}

pub async fn sign_with_jwt_priv_key(
    claims: PreferredUsernameClaims,
    key_filename: String,
) -> Result<String, Error> {
    let priv_key_data = match fs::read_to_string(key_filename.clone()) {
        Ok(data) => data,
        Err(err) => return Err(Error::JWTPrivKey(err.to_string())),
    };
    let priv_key_rsa = match Rsa::private_key_from_pem(priv_key_data.as_bytes()) {
        Ok(rsa) => rsa,
        Err(err) => return Err(Error::JWTPrivKey(err.to_string())),
    };
    let priv_key = match PKey::from_rsa(priv_key_rsa) {
        Ok(data) => data,
        Err(err) => return Err(Error::JWTPrivKey(err.to_string())),
    };
    let rs256_priv_key = PKeyWithDigest {
        digest: MessageDigest::sha256(),
        key: priv_key,
    };
    let data = match claims.sign_with_key(&rs256_priv_key) {
        Ok(data) => data,
        Err(err) => return Err(Error::JWTPrivKey(err.to_string())),
    };
    Ok(data)
}

fn load_jwt_pub_key_entry(key_filename: &str) -> Result<PKeyWithDigest<Public>, Error> {
    let pub_key_data = match fs::read_to_string(key_filename) {
        Ok(data) => data,
        Err(err) => return Err(Error::JWTPubKey(err.to_string())),
    };
    let pub_key = match PKey::public_key_from_pem(pub_key_data.as_ref()) {
        Ok(key) => key,
        Err(err) => return Err(Error::JWTPubKey(err.to_string())),
    };
    validate_supported_jwt_public_key(&pub_key, key_filename)?;
    let rs256_public_key = PKeyWithDigest {
        digest: MessageDigest::sha256(),
        key: pub_key,
    };
    Ok(rs256_public_key)
}

pub(crate) fn validate_supported_jwt_public_key(
    pub_key: &PKey<Public>,
    key_filename: &str,
) -> Result<(), Error> {
    match pub_key.id() {
        Id::RSA | Id::EC => Ok(()),
        id => Err(Error::JWTPubKey(format!(
            "unsupported JWT public key type {id:?} in {key_filename}; expected RSA or EC"
        ))),
    }
}

pub fn validate_jwt_pub_key_file(key_filename: &str) -> Result<(), Error> {
    load_jwt_pub_key_entry(key_filename).map(|_| ())
}

pub(crate) fn stage_jwt_pub_keys(key_filenames: Vec<String>) -> Result<StagedJwtPubKeys, Error> {
    let mut loaded = HashMap::with_capacity(key_filenames.len());
    for key_filename in key_filenames {
        let key = load_jwt_pub_key_entry(&key_filename)?;
        loaded.insert(key_filename, key);
    }
    let current_filenames: HashSet<String> = loaded.keys().cloned().collect();
    Ok(StagedJwtPubKeys {
        loaded,
        current_filenames,
    })
}

pub async fn publish_jwt_pub_keys(key_filenames: Vec<String>) -> Result<(), Error> {
    let staged = stage_jwt_pub_keys(key_filenames)?;
    publish_staged_jwt_pub_keys(staged).await;
    Ok(())
}

pub(crate) async fn jwt_pub_keys_write_guards() -> JwtPubKeysWriteGuards {
    JwtPubKeysWriteGuards {
        keys: KEYS.write().await,
        published_filenames: PUBLISHED_JWT_KEY_FILENAMES.write().await,
    }
}

pub(crate) fn publish_staged_jwt_pub_keys_locked(
    staged: StagedJwtPubKeys,
    guards: &mut JwtPubKeysWriteGuards,
) {
    for stale_filename in guards
        .published_filenames
        .difference(&staged.current_filenames)
    {
        guards.keys.remove(stale_filename);
    }
    for (key_filename, key) in staged.loaded {
        guards.keys.insert(key_filename, key);
    }
    *guards.published_filenames = staged.current_filenames;
}

pub(crate) async fn publish_staged_jwt_pub_keys(staged: StagedJwtPubKeys) {
    let mut guards = jwt_pub_keys_write_guards().await;
    publish_staged_jwt_pub_keys_locked(staged, &mut guards);
}

pub async fn load_jwt_pub_key(key_filename: String) -> Result<(), Error> {
    let rs256_public_key = load_jwt_pub_key_entry(&key_filename)?;
    let mut guard_write = KEYS.write().await;
    guard_write.insert(key_filename, rs256_public_key);
    Ok(())
}

pub async fn get_user_name_from_jwt(
    key_filename: String,
    input_token: String,
    expected_issuer: &str,
    expected_audience: &str,
) -> Result<String, Error> {
    let read_guard = KEYS.read().await;
    let pub_key = match read_guard.get(&key_filename) {
        Some(key) => key,
        None => return Err(Error::JWTPubKey("key is not loaded".to_string())),
    };
    let token: Token<Header, PreferredUsernameClaims, _> =
        match VerifyWithKey::verify_with_key(input_token.as_str(), pub_key) {
            Ok(token) => token,
            Err(err) => return Err(Error::JWTValidate(err.to_string())),
        };
    let (_, claim) = token.into();
    claim.validate(expected_issuer, expected_audience)?;
    Ok(claim.username)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jwt::{AlgorithmType, SignWithKey};
    use openssl::dsa::Dsa;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // Helper function to generate temporary RSA key pair
    fn generate_temp_rsa_keys() -> (NamedTempFile, NamedTempFile) {
        // Generate RSA key pair
        let rsa = Rsa::generate(2048).unwrap();

        // Create private key PEM
        let private_pem = rsa.private_key_to_pem().unwrap();
        let mut private_file = NamedTempFile::new().unwrap();
        private_file.write_all(&private_pem).unwrap();
        private_file.flush().unwrap();

        // Create public key PEM
        let public_pem = rsa.public_key_to_pem().unwrap();
        let mut public_file = NamedTempFile::new().unwrap();
        public_file.write_all(&public_pem).unwrap();
        public_file.flush().unwrap();

        (private_file, public_file)
    }

    fn generate_temp_dsa_public_key() -> NamedTempFile {
        let dsa = Dsa::generate(2048).unwrap();
        let public_pem = dsa.public_key_to_pem().unwrap();
        let mut public_file = NamedTempFile::new().unwrap();
        public_file.write_all(&public_pem).unwrap();
        public_file.flush().unwrap();
        public_file
    }

    #[test]
    fn validate_jwt_pub_key_file_rejects_unsupported_key_type() {
        let public_file = generate_temp_dsa_public_key();
        let public_path = public_file.path().to_str().unwrap();

        let err = validate_jwt_pub_key_file(public_path).unwrap_err();

        assert!(
            err.to_string().contains("unsupported JWT public key type"),
            "unexpected unsupported-key validation error: {err}"
        );
    }

    #[tokio::test]
    async fn test_token() {
        let (private_file, public_file) = generate_temp_rsa_keys();
        let private_path = private_file.path().to_str().unwrap().to_string();
        let public_path = public_file.path().to_str().unwrap().to_string();

        load_jwt_pub_key(public_path.clone()).await.unwrap();
        let private_pem = fs::read_to_string(&private_path).unwrap();
        let rs256_private_key = PKeyWithDigest {
            digest: MessageDigest::sha256(),
            key: PKey::private_key_from_pem(private_pem.as_ref()).unwrap(),
        };
        let header = Header {
            algorithm: AlgorithmType::Rs256,
            ..Default::default()
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut claims = new_claims_with_scope(
            "test".to_string(),
            Duration::from_secs(2),
            "issuer".to_string(),
            "audience".to_string(),
        );
        claims.default_claims.expiration = Some(now + 2);
        let signed_token = Token::new(header, claims)
            .sign_with_key(&rs256_private_key)
            .unwrap();
        let token_str = signed_token.as_str();
        get_user_name_from_jwt(public_path, token_str.to_string(), "issuer", "audience")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_generate_and_validate() {
        let (private_file, public_file) = generate_temp_rsa_keys();
        let private_path = private_file.path().to_str().unwrap().to_string();
        let public_path = public_file.path().to_str().unwrap().to_string();

        let username = "test";
        let claims = new_claims_with_scope(
            username.to_string(),
            Duration::from_secs(2),
            "issuer".to_string(),
            "audience".to_string(),
        );
        let token = match sign_with_jwt_priv_key(claims, private_path).await {
            Ok(token) => token,
            Err(err) => panic!("{err:?}"),
        };
        load_jwt_pub_key(public_path.clone()).await.unwrap();
        let token_username =
            match get_user_name_from_jwt(public_path, token, "issuer", "audience").await {
                Ok(username) => username,
                Err(err) => panic!("{err:?}"),
            };
        assert_eq!(username, token_username);
    }

    #[tokio::test]
    async fn jwt_validation_rejects_missing_scope() {
        let (private_file, public_file) = generate_temp_rsa_keys();
        let private_path = private_file.path().to_str().unwrap().to_string();
        let public_path = public_file.path().to_str().unwrap().to_string();
        load_jwt_pub_key(public_path.clone()).await.unwrap();

        let token = sign_with_jwt_priv_key(
            new_claims("test".to_string(), Duration::from_secs(60)),
            private_path,
        )
        .await
        .unwrap();

        let err = get_user_name_from_jwt(public_path, token, "issuer", "audience")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("issuer") || err.to_string().contains("audience"),
            "unexpected JWT scope error: {err}"
        );
    }

    #[tokio::test]
    async fn jwt_validation_rejects_wrong_audience() {
        let (private_file, public_file) = generate_temp_rsa_keys();
        let private_path = private_file.path().to_str().unwrap().to_string();
        let public_path = public_file.path().to_str().unwrap().to_string();
        load_jwt_pub_key(public_path.clone()).await.unwrap();

        let token = sign_with_jwt_priv_key(
            new_claims_with_scope(
                "test".to_string(),
                Duration::from_secs(60),
                "issuer".to_string(),
                "other-audience".to_string(),
            ),
            private_path,
        )
        .await
        .unwrap();

        let err = get_user_name_from_jwt(public_path, token, "issuer", "audience")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("audience"),
            "unexpected JWT audience error: {err}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(jwt_keys)]
    async fn publish_jwt_pub_keys_prunes_removed_reload_keys() {
        let (_old_private, old_public) = generate_temp_rsa_keys();
        let (_new_private, new_public) = generate_temp_rsa_keys();
        let old_path = old_public.path().to_str().unwrap().to_string();
        let new_path = new_public.path().to_str().unwrap().to_string();

        {
            let mut keys = KEYS.write().await;
            keys.clear();
        }
        {
            let mut published = PUBLISHED_JWT_KEY_FILENAMES.write().await;
            published.clear();
        }

        publish_jwt_pub_keys(vec![old_path.clone(), new_path.clone()])
            .await
            .unwrap();
        publish_jwt_pub_keys(vec![new_path.clone()]).await.unwrap();

        let keys = KEYS.read().await;
        assert!(
            !keys.contains_key(&old_path),
            "removed JWT public key must not remain trusted after reload publish"
        );
        assert!(
            keys.contains_key(&new_path),
            "current JWT public key must remain trusted after reload publish"
        );
    }
}

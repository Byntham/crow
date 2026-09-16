//! GitHub API boundary, application authentication and delivery recovery.
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use reqwest::{Client, Method, header::HeaderMap};
use rsa::{
    RsaPrivateKey,
    pkcs1::DecodeRsaPrivateKey,
    pkcs1v15::SigningKey,
    pkcs8::DecodePrivateKey,
    signature::{RandomizedSigner, SignatureEncoding},
};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{collections::HashSet, time::Duration};

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct GitHubError {
    pub message: String,
    pub status: u16,
    /// Server-requested retry delay in milliseconds.
    pub retry_after: u64,
}

pub fn verify(raw: &[u8], signature: Option<&str>, secret: Option<&str>) -> bool {
    let Some(secret) = secret.filter(|s| !s.is_empty()) else {
        return false;
    };
    let Some(signature) = signature else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(raw);
    crate::util::equal(
        &format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
        signature,
    )
}

pub fn jwt(app: &Value) -> Result<String> {
    let id = match &app["id"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => bail!("Invalid GitHub App identifier"),
    };
    let pem = app["pem"]
        .as_str()
        .context("Invalid GitHub App private key")?;
    let key = RsaPrivateKey::from_pkcs8_pem(pem)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
        .context("Invalid GitHub App private key")?;
    let now = crate::util::now() / 1000;
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(
        &json!({"iat":now-30,"exp":now+540,"iss":id}),
    )?);
    let unsigned = format!("{header}.{claims}");
    let signature = SigningKey::<Sha256>::new(key)
        .try_sign_with_rng(&mut rand::thread_rng(), unsigned.as_bytes())
        .context("Cannot sign GitHub App JWT")?;
    Ok(format!(
        "{unsigned}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

fn object(v: &Value) -> Result<&serde_json::Map<String, Value>> {
    v.as_object().context("Unexpected GitHub object")
}
fn string(v: &Value) -> Result<&str> {
    v.as_str().context("Unexpected GitHub string")
}
fn integer(v: &Value) -> Result<i64> {
    let n = v.as_f64().context("Unexpected GitHub integer")?;
    if n.fract() != 0.0 || n.abs() > 9_007_199_254_740_991.0 {
        bail!("Unexpected GitHub integer");
    }
    Ok(n as i64)
}
fn boolean(v: &Value) -> Result<bool> {
    v.as_bool().context("Unexpected GitHub boolean")
}
fn pull_request(v: Value) -> Result<Value> {
    let p = object(&v)?;
    object(&v["user"])?;
    object(&v["head"])?;
    object(&v["base"])?;
    let mut result = json!({"number":integer(&v["number"])? ,"state":string(&v["state"])? ,"draft":match p.get("draft") {Some(v)=>boolean(v)?,None=>false},"user":{"login":string(&v["user"]["login"])?},"head":{"sha":string(&v["head"]["sha"])?},"base":{"sha":string(&v["base"]["sha"])? ,"ref":string(&v["base"]["ref"])?}});
    for key in ["title", "created_at", "updated_at"] {
        if v[key].is_string() {
            result[key] = v[key].clone();
        }
    }
    if p.get("body").is_some_and(|b| b.is_string() || b.is_null()) {
        result["body"] = v["body"].clone();
    }
    Ok(result)
}
fn author(v: &Value) -> Result<Value> {
    if v.is_null() {
        Ok(Value::Null)
    } else {
        object(v)?;
        Ok(json!({"id":integer(&v["id"])?}))
    }
}
fn comment(v: Value) -> Result<Value> {
    let obj = object(&v)?;
    let user = obj.get("user").context("Unexpected GitHub object")?;
    Ok(
        json!({"id":integer(&v["id"])? ,"body":match obj.get("body") {Some(b)=>string(b)?,None=>""},"user":author(user)?}),
    )
}
fn review(v: Value) -> Result<Value> {
    let obj = object(&v)?;
    let body = obj.get("body").context("Unexpected GitHub string")?;
    let user = obj.get("user").context("Unexpected GitHub object")?;
    Ok(
        json!({"id":integer(&v["id"])? ,"body":if body.is_null() {""} else {string(body)?},"user":author(user)? ,"html_url":string(&v["html_url"])?}),
    )
}
// Delivery identifiers now exceed JavaScript's safe integer range. Keep them
// as JSON unsigned integers throughout parsing and request-path construction.
fn delivery_id(value: &Value) -> Result<u64> {
    value
        .as_u64()
        .filter(|id| *id > 0)
        .context("Invalid GitHub delivery identifier")
}
fn delivery(v: Value) -> Result<Value> {
    object(&v)?;
    Ok(
        json!({"id":delivery_id(&v["id"])? ,"guid":string(&v["guid"])? ,"status_code":integer(&v["status_code"])? ,"redelivery":boolean(&v["redelivery"])? ,"delivered_at":string(&v["delivered_at"])?}),
    )
}

/// Injectable network boundary; focused mocks need only implement methods they use.
#[async_trait]
#[allow(unused_variables)]
pub trait GitHubApi: Send + Sync {
    async fn request(
        &self,
        path: &str,
        token: Option<&str>,
        method: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        bail!("GitHub request is not implemented")
    }
    async fn token(&self, repo: &Value) -> Result<String> {
        bail!("GitHub token is not implemented")
    }
    async fn pr(&self, repo: &Value, n: i64, token: &str) -> Result<Value> {
        bail!("GitHub pr is not implemented")
    }
    async fn prs(&self, repo: &Value, token: &str) -> Result<Vec<Value>> {
        bail!("GitHub prs is not implemented")
    }
    async fn comments(&self, repo: &Value, n: i64, token: &str) -> Result<Vec<Value>> {
        bail!("GitHub comments is not implemented")
    }
    async fn reviews(&self, repo: &Value, n: i64, token: &str) -> Result<Vec<Value>> {
        bail!("GitHub reviews is not implemented")
    }
    async fn bot_id(&self) -> Result<i64> {
        bail!("GitHub bot_id is not implemented")
    }
    async fn installation(&self, name: &str) -> Result<Value> {
        bail!("GitHub installation is not implemented")
    }
    async fn status(
        &self,
        repo: &Value,
        n: i64,
        token: &str,
        body: &str,
        bot_id: i64,
        known_id: Option<i64>,
    ) -> Result<Value> {
        bail!("GitHub status is not implemented")
    }
    async fn publish(
        &self,
        repo: &Value,
        n: i64,
        token: &str,
        body: &str,
        head: &str,
        comments: &[Value],
    ) -> Result<Value> {
        bail!("GitHub publish is not implemented")
    }
    async fn audit(&self) -> Result<()> {
        bail!("GitHub audit is not implemented")
    }
}

#[derive(Clone)]
pub struct GitHub {
    pub app: Option<Value>,
    client: Client,
    pub origin: String,
}
impl GitHub {
    pub fn new(app: Option<Value>) -> Self {
        Self::with_origin(app, "https://api.github.com")
    }
    pub fn with_origin(app: Option<Value>, origin: impl Into<String>) -> Self {
        Self {
            app,
            origin: origin.into().trim_end_matches('/').into(),
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("TLS client initialization"),
        }
    }
    fn require_app(&self) -> Result<&Value> {
        self.app.as_ref().context("Complete GitHub App setup first")
    }
    pub fn path(&self, repo: &Value) -> Result<String> {
        let name = if repo.is_string() {
            string(repo)?
        } else {
            string(&repo["name"])?
        };
        Ok(format!("/repos/{}", crate::util::repo_name(name)?))
    }
    async fn request_headers(
        &self,
        path: &str,
        token: Option<&str>,
        method: &str,
        body: Option<&Value>,
    ) -> Result<(Value, HeaderMap)> {
        let mut req = self
            .client
            .request(
                Method::from_bytes(method.as_bytes())?,
                format!("{}{path}", self.origin),
            )
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "Crow/0.2")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = token.filter(|t| !t.is_empty()) {
            req = req.bearer_auth(token);
        }
        if let Some(body) = body {
            req = req.json(body);
        }
        let response = req.send().await?;
        let status = response.status();
        let headers = response.headers().clone();
        if !status.is_success() {
            let retry_after = headers
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
                .map(|v| (v * 1000.0) as u64)
                .unwrap_or(0);
            return Err(GitHubError {
                message: format!(
                    "GitHub {method} {} returned {}",
                    path.split('?').next().unwrap_or(path),
                    status.as_u16()
                ),
                status: status.as_u16(),
                retry_after,
            }
            .into());
        }
        let data = if status.as_u16() == 204 {
            Value::Null
        } else {
            response.json().await?
        };
        Ok((data, headers))
    }
    pub async fn list(&self, path: &str, token: Option<&str>) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        for page in 1..=1000 {
            let data = self
                .request(
                    &format!(
                        "{path}{}per_page=100&page={page}",
                        if path.contains('?') { "&" } else { "?" }
                    ),
                    token,
                    "GET",
                    None,
                )
                .await?;
            let items = if data.is_array() {
                data.as_array()
            } else {
                let o = object(&data)?;
                o.get("repositories")
                    .or_else(|| o.get("installations"))
                    .and_then(Value::as_array)
            }
            .context("Unexpected GitHub collection")?;
            result.extend(items.iter().cloned());
            if items.len() < 100 {
                return Ok(result);
            }
        }
        bail!("GitHub pagination limit reached")
    }
    pub async fn deliveries(&self, token: &str) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        let mut path = "/app/hook/deliveries?per_page=100".to_owned();
        let origin = url::Url::parse(&self.origin)?;
        loop {
            if seen.len() >= 1000 || !seen.insert(path.clone()) {
                bail!("GitHub delivery pagination did not terminate");
            }
            let (data, headers) = self
                .request_headers(&path, Some(token), "GET", None)
                .await?;
            for v in data
                .as_array()
                .context("Unexpected GitHub delivery collection")?
            {
                result.push(delivery(v.clone())?);
            }
            let link = headers
                .get("link")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("");
            static NEXT: std::sync::LazyLock<regex::Regex> =
                std::sync::LazyLock::new(|| regex::Regex::new(r#";\s*rel="next""#).unwrap());
            let Some(next) = link.split(',').find(|part| NEXT.is_match(part)) else {
                return Ok(result);
            };
            let target = next
                .split_once('<')
                .and_then(|(_, s)| s.split_once('>'))
                .map(|(u, _)| u)
                .context("Invalid GitHub delivery pagination link")?;
            let next = origin
                .join(target)
                .context("Invalid GitHub delivery pagination link")?;
            if next.origin() != origin.origin()
                || next.path() != "/app/hook/deliveries"
                || !next.username().is_empty()
                || next.password().is_some()
            {
                bail!("Invalid GitHub delivery pagination origin");
            }
            path = next.path().to_owned();
            if let Some(q) = next.query() {
                path.push('?');
                path.push_str(q);
            }
        }
    }
}

#[async_trait]
impl GitHubApi for GitHub {
    async fn request(
        &self,
        path: &str,
        token: Option<&str>,
        method: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        Ok(self.request_headers(path, token, method, body).await?.0)
    }
    async fn token(&self, repo: &Value) -> Result<String> {
        let token = jwt(self.require_app()?)?;
        let name = string(&repo["name"])?;
        crate::util::repo_name(name)?;
        let installation = match &repo["installation"] {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => bail!("Invalid GitHub installation identifier"),
        };
        if installation.is_empty() || !installation.bytes().all(|c| c.is_ascii_digit()) {
            bail!("Invalid GitHub installation identifier");
        }
        let body = json!({"repositories":[name.split('/').nth(1).unwrap()],"permissions":{"contents":"read","pull_requests":"write","issues":"write","metadata":"read"}});
        let result = self
            .request(
                &format!("/app/installations/{installation}/access_tokens"),
                Some(&token),
                "POST",
                Some(&body),
            )
            .await?;
        object(&result)?;
        Ok(string(&result["token"])?.into())
    }
    async fn pr(&self, repo: &Value, n: i64, token: &str) -> Result<Value> {
        pull_request(
            self.request(
                &format!("{}/pulls/{n}", self.path(repo)?),
                Some(token),
                "GET",
                None,
            )
            .await?,
        )
    }
    async fn prs(&self, repo: &Value, token: &str) -> Result<Vec<Value>> {
        self.list(
            &format!("{}/pulls?state=open", self.path(repo)?),
            Some(token),
        )
        .await?
        .into_iter()
        .map(pull_request)
        .collect()
    }
    async fn comments(&self, repo: &Value, n: i64, token: &str) -> Result<Vec<Value>> {
        self.list(
            &format!("{}/issues/{n}/comments", self.path(repo)?),
            Some(token),
        )
        .await?
        .into_iter()
        .map(comment)
        .collect()
    }
    async fn reviews(&self, repo: &Value, n: i64, token: &str) -> Result<Vec<Value>> {
        self.list(
            &format!("{}/pulls/{n}/reviews", self.path(repo)?),
            Some(token),
        )
        .await?
        .into_iter()
        .map(review)
        .collect()
    }
    async fn bot_id(&self) -> Result<i64> {
        let token = jwt(self.require_app()?)?;
        let app = self.request("/app", Some(&token), "GET", None).await?;
        let slug = string(&app["slug"])?;
        let encoded: String = format!("{slug}[bot]")
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        let user = self
            .request(&format!("/users/{encoded}"), None, "GET", None)
            .await?;
        integer(&user["id"])
    }
    async fn installation(&self, name: &str) -> Result<Value> {
        let token = jwt(self.require_app()?)?;
        let result = self
            .request(
                &format!("{}/installation", self.path(&json!(name))?),
                Some(&token),
                "GET",
                None,
            )
            .await?;
        Ok(json!({"id":integer(&result["id"])?}))
    }
    async fn status(
        &self,
        repo: &Value,
        n: i64,
        token: &str,
        body: &str,
        bot_id: i64,
        known_id: Option<i64>,
    ) -> Result<Value> {
        let mut known_id = known_id.filter(|n| *n != 0);
        if known_id.is_none() {
            known_id = self
                .comments(repo, n, token)
                .await?
                .iter()
                .find(|c| {
                    c["user"]["id"].as_i64() == Some(bot_id)
                        && c["body"]
                            .as_str()
                            .is_some_and(|s| s.starts_with("<!-- crow-status:v1 -->"))
                })
                .and_then(|c| c["id"].as_i64());
        }
        let body = json!({"body":body});
        if let Some(id) = known_id {
            match self
                .request(
                    &format!("{}/issues/comments/{id}", self.path(repo)?),
                    Some(token),
                    "PATCH",
                    Some(&body),
                )
                .await
            {
                Ok(result) => return Ok(json!({"id":integer(&result["id"])?})),
                Err(e)
                    if e.downcast_ref::<GitHubError>()
                        .is_some_and(|e| e.status == 404) => {}
                Err(e) => return Err(e),
            }
        }
        let result = self
            .request(
                &format!("{}/issues/{n}/comments", self.path(repo)?),
                Some(token),
                "POST",
                Some(&body),
            )
            .await?;
        Ok(json!({"id":integer(&result["id"])?}))
    }
    async fn publish(
        &self,
        repo: &Value,
        n: i64,
        token: &str,
        body: &str,
        head: &str,
        comments: &[Value],
    ) -> Result<Value> {
        let body = json!({"commit_id":head,"event":"COMMENT","body":body,"comments":comments});
        let result = self
            .request(
                &format!("{}/pulls/{n}/reviews", self.path(repo)?),
                Some(token),
                "POST",
                Some(&body),
            )
            .await?;
        Ok(json!({"id":integer(&result["id"])? ,"html_url":string(&result["html_url"])?}))
    }
    async fn audit(&self) -> Result<()> {
        let token = jwt(self.require_app()?)?;
        let deliveries = self.deliveries(&token).await?;
        let recovered: HashSet<_> = deliveries
            .iter()
            .filter(|d| {
                d["status_code"]
                    .as_i64()
                    .is_some_and(|s| (200..300).contains(&s))
            })
            .filter_map(|d| d["guid"].as_str())
            .filter(|s| !s.is_empty())
            .collect();
        for d in &deliveries {
            let status = d["status_code"].as_i64().unwrap();
            let recent = chrono::DateTime::parse_from_rfc3339(string(&d["delivered_at"])?)
                .is_ok_and(|t| crate::util::now() - t.timestamp_millis() < 3 * 86_400_000);
            if (status == 0 || status >= 400)
                && d["redelivery"] == false
                && !recovered.contains(string(&d["guid"])?)
                && recent
            {
                let id = delivery_id(&d["id"])?;
                self.request(
                    &format!("/app/hook/deliveries/{id}/attempts"),
                    Some(&token),
                    "POST",
                    None,
                )
                .await?;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Bytes,
        extract::State,
        http::{HeaderMap, StatusCode, Uri},
        response::IntoResponse,
    };
    use rsa::{pkcs8::EncodePrivateKey, signature::Verifier};
    use std::{
        collections::VecDeque,
        sync::{Arc, LazyLock, Mutex},
    };
    type Response = (StatusCode, HeaderMap, Value);
    #[derive(Clone, Debug)]
    struct Call {
        path: String,
        method: String,
        headers: HeaderMap,
        body: Value,
    }
    #[derive(Clone, Default)]
    struct Mock {
        queue: Arc<Mutex<VecDeque<Response>>>,
        calls: Arc<Mutex<Vec<Call>>>,
    }
    impl Mock {
        fn push(&self, status: u16, headers: HeaderMap, body: Value) {
            self.queue.lock().unwrap().push_back((
                StatusCode::from_u16(status).unwrap(),
                headers,
                body,
            ));
        }
        fn json(&self, value: Value) {
            self.push(200, HeaderMap::new(), value);
        }
    }
    async fn endpoint(
        State(mock): State<Mock>,
        method: axum::http::Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        mock.calls.lock().unwrap().push(Call {
            path: uri.to_string(),
            method: method.to_string(),
            headers,
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        });
        let (status, headers, body) = mock
            .queue
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected GitHub request");
        (status, headers, axum::Json(body))
    }
    async fn mock(app: Option<Value>) -> (GitHub, Mock, tokio::task::JoinHandle<()>) {
        let state = Mock::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new().fallback(endpoint).with_state(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (GitHub::with_origin(app, origin), state, handle)
    }
    fn app() -> Value {
        static PEM: LazyLock<String> = LazyLock::new(|| {
            RsaPrivateKey::new(&mut rand::thread_rng(), 2048)
                .unwrap()
                .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
                .unwrap()
                .to_string()
        });
        json!({"id":123,"pem":*PEM})
    }
    fn repo() -> Value {
        json!({"name":"owner/project","installation":1})
    }
    fn valid_pr() -> Value {
        json!({"number":3,"state":"open","user":{"login":"operator"},"head":{"sha":"a".repeat(40)},"base":{"sha":"b".repeat(40),"ref":"main"},"body":null,"title":"Change"})
    }
    #[test]
    fn exact_bytes_hmac_and_rsa_jwt() {
        let raw = br#"{"event":"push"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(raw);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify(raw, Some(&signature), Some("secret")));
        assert!(!verify(b"different", Some(&signature), Some("secret")));
        assert!(!verify(raw, None, Some("secret")));
        assert!(!verify(raw, Some(&signature), Some("")));
        let app = app();
        let token = jwt(&app).unwrap();
        let parts: Vec<_> = token.split('.').collect();
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], "123");
        assert_eq!(
            claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
            570
        );
        let key = RsaPrivateKey::from_pkcs8_pem(app["pem"].as_str().unwrap()).unwrap();
        rsa::pkcs1v15::VerifyingKey::<Sha256>::new(key.to_public_key())
            .verify(
                format!("{}.{}", parts[0], parts[1]).as_bytes(),
                &rsa::pkcs1v15::Signature::try_from(
                    URL_SAFE_NO_PAD.decode(parts[2]).unwrap().as_slice(),
                )
                .unwrap(),
            )
            .unwrap();
    }
    #[tokio::test]
    async fn rejects_malformed_boundary_values_and_normalizes_optional_fields() {
        let (gh, m, h) = mock(None).await;
        m.json(json!({"number":3}));
        assert!(
            gh.pr(&repo(), 3, "token")
                .await
                .unwrap_err()
                .to_string()
                .contains("Unexpected GitHub")
        );
        m.json(valid_pr());
        let mut expected = valid_pr();
        expected["draft"] = json!(false);
        assert_eq!(gh.pr(&repo(), 3, "token").await.unwrap(), expected);
        let mut bad = valid_pr();
        bad["draft"] = json!("false");
        m.json(bad);
        assert!(gh.pr(&repo(), 3, "token").await.is_err());
        m.json(json!([{"id":1,"user":null}]));
        assert_eq!(
            gh.comments(&repo(), 3, "token").await.unwrap(),
            vec![json!({"id":1,"user":null,"body":""})]
        );
        m.json(json!([{"id":1,"user":null,"body":null,"html_url":"https://example.com/review"}]));
        assert_eq!(
            gh.reviews(&repo(), 3, "token").await.unwrap()[0]["body"],
            ""
        );
        m.json(json!([{"id":1,"body":"absent author"}]));
        assert!(gh.comments(&repo(), 3, "token").await.is_err());
        m.json(json!({"id":1}));
        assert!(
            gh.publish(&repo(), 3, "token", "body", "head", &[])
                .await
                .is_err()
        );
        assert!(
            gh.token(&repo())
                .await
                .unwrap_err()
                .to_string()
                .contains("Complete GitHub App setup first")
        );
        h.abort();
    }
    #[tokio::test]
    async fn token_is_repository_scoped_and_bot_identity_is_authenticated() {
        let (gh, m, h) = mock(Some(app())).await;
        m.json(json!({"token":"installation-token"}));
        assert_eq!(gh.token(&repo()).await.unwrap(), "installation-token");
        m.json(json!({"token":42}));
        assert!(
            gh.token(&repo())
                .await
                .unwrap_err()
                .to_string()
                .contains("Unexpected GitHub string")
        );
        m.json(json!({"slug":"crow"}));
        m.json(json!({"id":42}));
        assert_eq!(gh.bot_id().await.unwrap(), 42);
        let calls = m.calls.lock().unwrap();
        assert_eq!(
            calls[0].body,
            json!({"repositories":["project"],"permissions":{"contents":"read","pull_requests":"write","issues":"write","metadata":"read"}})
        );
        assert!(
            calls[0].headers["authorization"]
                .to_str()
                .unwrap()
                .starts_with("Bearer ey")
        );
        assert!(calls[2].headers.contains_key("authorization"));
        assert!(!calls[3].headers.contains_key("authorization"));
        assert_eq!(calls[3].path, "/users/crow%5Bbot%5D");
        h.abort();
    }
    #[tokio::test]
    async fn bearer_requests_never_follow_redirects() {
        let (gh, m, h) = mock(None).await;
        let mut headers = HeaderMap::new();
        headers.insert(
            "location",
            "https://attacker.example/collect".parse().unwrap(),
        );
        m.push(302, headers, Value::Null);
        let error = gh
            .request("/app", Some("app-secret"), "GET", None)
            .await
            .unwrap_err();
        assert_eq!(error.downcast_ref::<GitHubError>().unwrap().status, 302);
        assert_eq!(m.calls.lock().unwrap().len(), 1);
        h.abort();
    }
    #[tokio::test]
    async fn status_discovery_checks_author_and_recreates_deleted_comment() {
        let (gh, m, h) = mock(None).await;
        m.json(json!([{"id":1,"user":null,"body":"<!-- crow-status:v1 --> forged"},{"id":2,"user":{"id":999},"body":"<!-- crow-status:v1 --> forged"},{"id":3,"user":{"id":42},"body":"<!-- crow-status:v1 --> current"}]));
        m.push(404, HeaderMap::new(), json!({}));
        m.json(json!({"id":4}));
        assert_eq!(
            gh.status(&repo(), 3, "token", "updated", 42, None)
                .await
                .unwrap(),
            json!({"id":4})
        );
        let calls = m.calls.lock().unwrap();
        assert_eq!(calls[1].method, "PATCH");
        assert!(calls[1].path.ends_with("/issues/comments/3"));
        assert_eq!(calls[2].method, "POST");
        h.abort();
    }
    #[tokio::test]
    async fn pagination_and_rate_limit_errors_preserve_protocol() {
        let (gh, m, h) = mock(None).await;
        m.json(json!({"repositories":vec![json!({"id":1});100]}));
        m.json(json!({"repositories":[{"id":2}]}));
        assert_eq!(
            gh.list("/installation/repositories", Some("token"))
                .await
                .unwrap()
                .len(),
            101
        );
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "3".parse().unwrap());
        m.push(429, headers, json!({"secret":"must not be surfaced"}));
        let error = gh
            .request("/rate?secret=query", Some("token"), "GET", None)
            .await
            .unwrap_err();
        let error = error.downcast_ref::<GitHubError>().unwrap();
        assert_eq!(error.status, 429);
        assert_eq!(error.retry_after, 3000);
        assert!(!error.message.contains("secret"));
        assert!(
            m.calls.lock().unwrap()[1]
                .path
                .ends_with("per_page=100&page=2")
        );
        h.abort();
    }
    #[tokio::test]
    async fn delivery_pagination_rejects_cross_origin_credentials_and_cycles() {
        for link in [
            "https://attacker.example/app/hook/deliveries?cursor=secret".to_string(),
            "/app/hook/deliveries?per_page=100".to_string(),
            "/repos/owner/project".to_string(),
        ] {
            let (gh, m, h) = mock(None).await;
            let mut headers = HeaderMap::new();
            headers.insert("link", format!("<{link}>; rel=\"next\"").parse().unwrap());
            m.push(200, headers, json!([]));
            let error = gh.deliveries("app-token").await.unwrap_err();
            assert!(error.to_string().contains("pagination"));
            assert_eq!(m.calls.lock().unwrap().len(), 1);
            h.abort();
        }
    }
    #[test]
    fn delivery_identifiers_accept_full_unsigned_range_and_reject_malformed_values() {
        for id in [9_007_199_254_740_993u64, u64::MAX] {
            let parsed: Value = serde_json::from_str(&format!("{{\"id\":{id}}}")).unwrap();
            assert_eq!(delivery_id(&parsed["id"]).unwrap(), id);
        }
        for invalid in [
            Value::Null,
            json!("123"),
            json!(0),
            json!(-1),
            json!(1.5),
            json!(1e30),
        ] {
            assert!(delivery_id(&invalid).is_err());
        }
    }
    #[tokio::test]
    async fn audit_preserves_large_delivery_identifiers_without_float_rounding() {
        let (gh, m, h) = mock(Some(app())).await;
        let ids = [9_007_199_254_740_993u64, u64::MAX];
        m.json(json!(
            ids.iter()
                .enumerate()
                .map(|(index, id)| json!({
                    "id":id,"guid":format!("large-{index}"),"status_code":502,
                    "redelivery":false,"delivered_at":chrono::Utc::now().to_rfc3339()
                }))
                .collect::<Vec<_>>()
        ));
        for _ in ids {
            m.push(204, HeaderMap::new(), Value::Null);
        }
        gh.audit().await.unwrap();
        let calls = m.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        for (index, id) in ids.iter().enumerate() {
            assert_eq!(
                calls[index + 1].path,
                format!("/app/hook/deliveries/{id}/attempts")
            );
        }
        h.abort();
    }
    #[tokio::test]
    async fn audit_follows_cursor_and_skips_recovered_old_or_retried_deliveries() {
        let (gh, m, h) = mock(Some(app())).await;
        let make = |id, guid, status, redelivery, old| json!({"id":id,"guid":guid,"status_code":status,"redelivery":redelivery,"delivered_at":if old {"2001-01-01T00:00:00Z".to_string()}else{chrono::Utc::now().to_rfc3339()}});
        let mut headers = HeaderMap::new();
        headers.insert(
            "link",
            "</app/hook/deliveries?cursor=next>; rel=\"next\""
                .parse()
                .unwrap(),
        );
        m.push(
            200,
            headers,
            json!([
                make(1, "retry", 0, false, false),
                make(2, "recovered", 500, false, false),
                make(3, "old", 500, false, true),
                make(4, "redelivered", 500, true, false)
            ]),
        );
        m.json(json!([make(5, "recovered", 200, true, false)]));
        m.push(204, HeaderMap::new(), Value::Null);
        gh.audit().await.unwrap();
        let calls = m.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[1].path, "/app/hook/deliveries?cursor=next");
        assert_eq!(calls[2].path, "/app/hook/deliveries/1/attempts");
        h.abort();
    }
}

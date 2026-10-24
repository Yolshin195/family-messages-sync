//! Клиент REST API Семейной бухгалтерии (`Authorization: Bearer fbt_…`).

use anyhow::{Context, Result, anyhow, bail};
use chrono::NaiveDate;
use reqwest::{
    Method,
    blocking::{Client, ClientBuilder},
};
use serde_json::{Value, json};
use std::time::Duration;

use crate::{
    config::{Category, Config},
    dedupe::Existing,
    keychain,
};

/// Ответ старой бухгалтерии, которая не знает связей SMS с операциями.
pub const OLD_BUDGET: &str = "Обновите Семейную бухгалтерию: нужна версия со связями SMS (docs/external-refs.md)";

pub struct Api {
    http: Client,
    base: String,
    token: String,
    /// `provider` связей SMS
    provider: String,
}

/// Ответ бухгалтерии на создание операции или прикрепление SMS. Ошибки доступа, сети и сервера — `Err`:
/// остановка, повтор в следующий запуск.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// создана новая операция
    Created(String),
    /// SMS прикреплено к этой операции
    Matched(String),
    /// операция по этому SMS уже есть
    AlreadySent(String),
    /// операцию по этому SMS удалили в бухгалтерии (id, если удалось узнать)
    Deleted(Option<String>),
    /// кандидата на пару удалили, пока шла обработка
    Gone,
    /// бухгалтерия отклонила именно это SMS (нет счёта, категория не того типа): повтор без исправления не поможет
    Rejected(String),
}

/// Бухгалтерия глазами обработки сообщений (в тестах подменяется).
pub trait Ledger {
    /// Операции по счёту за период, включая переводы с этого и на этот счёт.
    fn transactions_around(&self, account: &str, from: NaiveDate, to: NaiveDate) -> Result<Vec<Existing>>;
    /// Создать операцию; в `payload` есть `external_ref`.
    fn create(&self, payload: &Value) -> Result<Reply>;
    /// Прикрепить SMS к операции `id`.
    fn attach(&self, id: &str, external_ref: &Value) -> Result<Reply>;
}

/// Связь из `GET /api/transactions/external-refs`.
struct Link {
    transaction_id: String,
    transaction_exists: bool,
}

pub struct AccountInfo {
    pub currency: String,
    pub archived: bool,
}

/// Код ошибки из тела `{"error": "<КОД>", "message": "…", "field": "…"}`.
fn error_code(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body).ok()?["error"].as_str().map(str::to_string)
}

/// Текст отказа для журнала: `message` (и `field`) из тела, иначе тело целиком.
fn error_text(body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    match (v["message"].as_str(), v["field"].as_str()) {
        (Some(m), Some(f)) => format!("{m} ({f})"),
        (Some(m), None) => m.to_string(),
        _ => body.trim().to_string(),
    }
}

fn id_of(path: &str, body: &str) -> Result<String> {
    let v: Value = serde_json::from_str(body).with_context(|| format!("{path}: ответ не JSON: {body}"))?;
    Ok(v["id"].as_str().with_context(|| format!("{path}: в ответе нет id: {body}"))?.to_string())
}

impl Api {
    pub fn new(cfg: &Config) -> Result<Self> {
        let token = keychain::get(&cfg.keychain_service)?;
        if !token.starts_with("fbt_") {
            bail!(
                "Это не похоже на API-токен: он начинается с fbt_. Создайте его в «Профиль → Безопасность» и выполните set-token"
            );
        }
        Self::connect(cfg, token, Client::builder())
    }

    fn connect(cfg: &Config, token: String, http: ClientBuilder) -> Result<Self> {
        Ok(Self {
            http: http.timeout(Duration::from_secs(20)).build()?,
            base: cfg.api_url.trim_end_matches('/').to_string(),
            token,
            provider: cfg.provider.clone(),
        })
    }

    fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<(u16, String)> {
        let mut rb = self.http.request(method, format!("{}{}", self.base, path)).bearer_auth(&self.token);
        if !query.is_empty() {
            rb = rb.query(query);
        }
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let resp = rb.send().with_context(|| format!("запрос {path} (проверьте api_url и доступность бухгалтерии)"))?;
        Ok((resp.status().as_u16(), resp.text()?))
    }

    /// Ошибки доступа, одинаковые для всех запросов.
    fn access_error(path: &str, status: u16, body: &str) -> Option<anyhow::Error> {
        match status {
            401 => Some(anyhow!(
                "{path}: 401 токен недействителен, истёк или отозван. Создайте новый и выполните set-token"
            )),
            403 => Some(anyhow!(
                "{path}: 403 у токена нет нужного права ({body}). Для `run` достаточно transactions:write, для `init` нужен write: \
                 выдайте его на время настройки или создайте категории в приложении вручную"
            )),
            429 => Some(anyhow!("{path}: 429 слишком много запросов, повторим позже")),
            s if s >= 500 => Some(anyhow!("{path}: {s} ошибка сервера: {body}")),
            _ => None,
        }
    }

    /// Запрос с проверкой ошибок доступа.
    fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<(u16, String)> {
        let (st, text) = self.call(method, path, query, body)?;
        match Self::access_error(path, st, &text) {
            Some(e) => Err(e),
            None => Ok((st, text)),
        }
    }

    fn get_json(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let (st, body) = self.send(Method::GET, path, query, None)?;
        if st != 200 {
            bail!("{path}: {st} {body}");
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// Все страницы списка (`content` + `page.totalPages`).
    fn get_all(&self, path: &str, query: &[(&str, String)]) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for page in 0..50 {
            let mut q = query.to_vec();
            q.push(("page", page.to_string()));
            q.push(("size", "200".into()));
            let v = self.get_json(path, &q)?;
            out.extend(v["content"].as_array().cloned().unwrap_or_default());
            if page + 1 >= v["page"]["totalPages"].as_i64().unwrap_or(1) {
                break;
            }
        }
        Ok(out)
    }

    /// Связь SMS с операцией; `None` — связи нет.
    fn find_link(&self, external_id: &str) -> Result<Option<Link>> {
        let path = "/api/transactions/external-refs";
        let q = [("provider", self.provider.clone()), ("external_id", external_id.to_string())];
        let (st, body) = self.send(Method::GET, path, &q, None)?;
        match st {
            200 => {
                let v: Value = serde_json::from_str(&body).with_context(|| format!("{path}: ответ не JSON: {body}"))?;
                let id = v["transaction_id"]
                    .as_str()
                    .with_context(|| format!("{path}: в ответе нет transaction_id: {body}"))?;
                Ok(Some(Link {
                    transaction_id: id.to_string(),
                    transaction_exists: v["transaction_exists"].as_bool().unwrap_or(true),
                }))
            }
            404 => Ok(None),
            _ => bail!("{path}: {st} {body}"),
        }
    }

    /// Отказ бухгалтерии: SMS по удалённой операции не заводится, прочие 4xx — в очередь повторов.
    fn refused(&self, path: &str, st: u16, body: &str, external_ref: &Value) -> Result<Reply> {
        match st {
            408 => bail!("{path}: 408 таймаут"),
            409 if error_code(body).as_deref() == Some("EXTERNAL_REF_DELETED") => {
                // id удалённой операции нужен только журналу: не удалось узнать — не беда
                let ext = external_ref["external_id"].as_str().unwrap_or_default();
                Ok(Reply::Deleted(self.find_link(ext).ok().flatten().map(|l| l.transaction_id)))
            }
            400..=499 => Ok(Reply::Rejected(format!("{st} {}", error_text(body)))),
            _ => bail!("{path}: {st} {body}"),
        }
    }

    /// Знает ли бухгалтерия связи SMS: у старой версии путь попадает в `/{id}` и не разбирается как UUID (400).
    pub fn check_external_refs(&self) -> Result<()> {
        let q = [("provider", self.provider.clone()), ("external_id", "probe".to_string())];
        let (st, body) = self.send(Method::GET, "/api/transactions/external-refs", &q, None)?;
        match st {
            200 => Ok(()),
            404 if error_code(&body).as_deref() == Some("NOT_FOUND") => Ok(()),
            _ => bail!("{OLD_BUDGET} (проверка связей: {st} {})", error_text(&body)),
        }
    }

    pub fn account_info(&self, code: &str) -> Result<Option<AccountInfo>> {
        let path = format!("/api/accounts/{code}");
        let (st, body) = self.send(Method::GET, &path, &[], None)?;
        match st {
            200 => {
                let v: Value = serde_json::from_str(&body)?;
                Ok(Some(AccountInfo {
                    currency: v["currency_code"].as_str().unwrap_or("").to_string(),
                    archived: v["archived"].as_bool().unwrap_or(false),
                }))
            }
            404 => Ok(None),
            _ => bail!("{path}: {st} {body}"),
        }
    }

    /// Создаёт категорию, если её нет. `kind`: EXPENSE | INCOME.
    pub fn ensure_category(&self, cat: &Category, kind: &str) -> Result<()> {
        let code = cat.code.trim().to_uppercase();
        let all = self.get_all("/api/categories", &[("include_archived", "true".into())])?;
        if let Some(c) = all.iter().find(|c| c["code"].as_str() == Some(code.as_str())) {
            if c["category_kind"].as_str() != Some(kind) {
                bail!("категория {code} уже есть, но это не {kind}: создайте другую и укажите её код в конфиге");
            }
            if c["archived"].as_bool() == Some(true) {
                bail!("категория {code} в архиве: верните её из архива в приложении");
            }
            println!("категория {code}: уже есть");
            return Ok(());
        }
        let payload = json!({
            "code": code, "name": cat.name, "name_lang": "RU", "name_en": cat.name_en,
            "category_kind": kind, "parent_code": null, "sort_order": 900
        });
        let (st, body) = self.call(Method::POST, "/api/categories", &[], Some(&payload))?;
        match st {
            200 | 201 => println!("категория {code}: создана"),
            403 => bail!(
                "категория {code} не создана: у токена нет права записи в категории. Создайте её в приложении (тип {kind}) или выдайте токену право write"
            ),
            _ => bail!("категория {code}: {st} {body}"),
        }
        Ok(())
    }
}

impl Ledger for Api {
    fn transactions_around(&self, account: &str, from: NaiveDate, to: NaiveDate) -> Result<Vec<Existing>> {
        let q = [("from", from.to_string()), ("to", to.to_string()), ("account_code", account.to_string())];
        let all = self.get_all("/api/transactions", &q)?;
        // без связей не отличить уже отправленные SMS от новых
        if !all.is_empty() && !all.iter().any(|t| t.get("external_refs").is_some()) {
            bail!(OLD_BUDGET);
        }
        Ok(all.iter().filter_map(|t| Existing::from_json(t, &self.provider)).collect())
    }

    fn create(&self, payload: &Value) -> Result<Reply> {
        let path = "/api/transactions";
        let (st, body) = self.send(Method::POST, path, &[], Some(payload))?;
        match st {
            201 => Ok(Reply::Created(id_of(path, &body)?)),
            // прошлый запуск создал операцию, но ответ не дошёл
            200 => Ok(Reply::AlreadySent(id_of(path, &body)?)),
            _ => self.refused(path, st, &body, &payload["external_ref"]),
        }
    }

    fn attach(&self, id: &str, external_ref: &Value) -> Result<Reply> {
        let path = format!("/api/transactions/{id}/external-refs");
        let (st, body) = self.send(Method::POST, &path, &[], Some(external_ref))?;
        match st {
            200 | 201 => Ok(Reply::Matched(id.to_string())),
            404 => Ok(Reply::Gone),
            409 if error_code(&body).as_deref() == Some("EXTERNAL_REF_TAKEN") => {
                // SMS уже прикреплено к другой операции (второй Mac, сброс состояния): к какой?
                let ext = external_ref["external_id"].as_str().unwrap_or_default();
                match self.find_link(ext)? {
                    Some(l) if l.transaction_exists => Ok(Reply::AlreadySent(l.transaction_id)),
                    Some(l) => Ok(Reply::Deleted(Some(l.transaction_id))),
                    None => bail!("{path}: 409 {}, но связь SMS {ext} не найдена", error_text(&body)),
                }
            }
            _ => self.refused(&path, st, &body, external_ref),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
    };

    /// Бухгалтерия на 127.0.0.1: на каждый запрос — следующий ответ из списка.
    /// В канал уходят запросы: "МЕТОД путь?запрос тело".
    fn serve(replies: Vec<(u16, &'static str)>) -> (Api, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = Config { api_url: format!("http://{}/", listener.local_addr().unwrap()), ..Config::default() };
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for (status, reply) in replies {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut len = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let target = line.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
                // тест может не читать запросы
                let _ = tx.send(format!("{target} {}", String::from_utf8_lossy(&body)).trim_end().to_string());
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
        });
        (Api::connect(&cfg, "fbt_test".into(), Client::builder().no_proxy()).unwrap(), rx)
    }

    fn ext() -> Value {
        json!({"provider": "imessage", "external_id": "G-1", "received_at": "2026-09-21T10:51:07+07:00"})
    }

    fn payload() -> Value {
        json!({"tx_kind": "EXPENSE", "external_ref": ext()})
    }

    const LINK_GONE: &str =
        r#"{"provider":"imessage","external_id":"G-1","transaction_id":"t9","transaction_exists":false}"#;
    const LINK_LIVE: &str =
        r#"{"provider":"imessage","external_id":"G-1","transaction_id":"t9","transaction_exists":true}"#;
    const NOT_FOUND: &str = r#"{"error":"NOT_FOUND","message":"не найдено"}"#;

    #[test]
    fn create_responses() {
        let (api, rx) = serve(vec![(201, r#"{"id":"t1"}"#)]);
        assert_eq!(api.create(&payload()).unwrap(), Reply::Created("t1".into()));
        let req = rx.recv().unwrap();
        assert!(req.starts_with("POST /api/transactions {") && req.contains(r#""external_id":"G-1""#), "{req}");

        let (api, _) = serve(vec![(200, r#"{"id":"t0"}"#)]);
        assert_eq!(api.create(&payload()).unwrap(), Reply::AlreadySent("t0".into()));

        let (api, rx) = serve(vec![(409, r#"{"error":"EXTERNAL_REF_DELETED","message":"удалена"}"#), (200, LINK_GONE)]);
        assert_eq!(api.create(&payload()).unwrap(), Reply::Deleted(Some("t9".into())));
        assert_eq!(rx.iter().nth(1).unwrap(), "GET /api/transactions/external-refs?provider=imessage&external_id=G-1");

        // id удалённой операции узнать не удалось — всё равно не заводим
        let (api, _) = serve(vec![(409, r#"{"error":"EXTERNAL_REF_DELETED"}"#), (500, "{}")]);
        assert_eq!(api.create(&payload()).unwrap(), Reply::Deleted(None));

        let (api, _) = serve(vec![(422, r#"{"error":"VALIDATION","message":"категория AUTO_INCOME не найдена"}"#)]);
        assert_eq!(api.create(&payload()).unwrap(), Reply::Rejected("422 категория AUTO_INCOME не найдена".into()));

        let (api, _) = serve(vec![(409, r#"{"error":"CONFLICT","message":"что-то ещё","field":"items"}"#)]);
        assert_eq!(api.create(&payload()).unwrap(), Reply::Rejected("409 что-то ещё (items)".into()));

        for (st, what) in [(500, "500"), (401, "401"), (403, "403"), (408, "408"), (429, "429")] {
            let (api, _) = serve(vec![(st, "{}")]);
            let e = api.create(&payload()).unwrap_err().to_string();
            assert!(e.contains(what), "{st}: {e}");
        }
    }

    #[test]
    fn attach_responses() {
        for st in [201, 200] {
            let (api, rx) = serve(vec![(st, "{}")]);
            assert_eq!(api.attach("a", &ext()).unwrap(), Reply::Matched("a".into()));
            assert!(rx.recv().unwrap().starts_with("POST /api/transactions/a/external-refs {"));
        }

        let (api, _) = serve(vec![(409, r#"{"error":"EXTERNAL_REF_TAKEN"}"#), (200, LINK_LIVE)]);
        assert_eq!(api.attach("a", &ext()).unwrap(), Reply::AlreadySent("t9".into()));

        let (api, _) = serve(vec![(409, r#"{"error":"EXTERNAL_REF_TAKEN"}"#), (200, LINK_GONE)]);
        assert_eq!(api.attach("a", &ext()).unwrap(), Reply::Deleted(Some("t9".into())));

        let (api, _) = serve(vec![(409, r#"{"error":"EXTERNAL_REF_TAKEN"}"#), (404, NOT_FOUND)]);
        assert!(api.attach("a", &ext()).is_err());

        let (api, _) = serve(vec![(404, NOT_FOUND)]);
        assert_eq!(api.attach("a", &ext()).unwrap(), Reply::Gone);

        let (api, _) = serve(vec![(422, r#"{"message":"нельзя"}"#)]);
        assert_eq!(api.attach("a", &ext()).unwrap(), Reply::Rejected("422 нельзя".into()));

        let (api, _) = serve(vec![(503, "{}")]);
        assert!(api.attach("a", &ext()).is_err());
    }

    #[test]
    fn transactions_need_external_refs() {
        let page = |items: &str| -> &'static str {
            Box::leak(format!(r#"{{"content":[{items}],"page":{{"totalPages":1}}}}"#).into_boxed_str())
        };
        let tx = r#"{"id":"a","tx_kind":"EXPENSE","account_code":"CARD","total":"60.00","occurred_on":"2026-09-21","items":[]"#;
        let d = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();

        let (api, rx) = serve(vec![(
            200,
            page(&format!(r#"{tx},"external_refs":[{{"provider":"imessage","external_id":"G-1"}}]}}"#)),
        )]);
        let got = api.transactions_around("CARD", d, d).unwrap();
        assert_eq!(got[0].sms_guids, ["G-1"]);
        let req = rx.recv().unwrap();
        assert!(
            req.starts_with("GET /api/transactions?from=2026-09-21&to=2026-09-21&account_code=CARD&page=0&size=200"),
            "{req}"
        );

        let (api, _) = serve(vec![(200, page(&format!("{tx}}}")))]);
        assert_eq!(api.transactions_around("CARD", d, d).unwrap_err().to_string(), OLD_BUDGET);

        let (api, _) = serve(vec![(200, page(""))]);
        assert!(api.transactions_around("CARD", d, d).unwrap().is_empty());
    }

    #[test]
    fn init_checks_budget_version() {
        let (api, rx) = serve(vec![(404, NOT_FOUND)]);
        api.check_external_refs().unwrap();
        assert_eq!(rx.recv().unwrap(), "GET /api/transactions/external-refs?provider=imessage&external_id=probe");
        let (api, _) = serve(vec![(200, LINK_LIVE)]);
        api.check_external_refs().unwrap();
        let (api, _) = serve(vec![(400, r#"{"error":"BAD_REQUEST","message":"invalid UUID"}"#)]);
        assert!(api.check_external_refs().unwrap_err().to_string().starts_with(OLD_BUDGET));
        let (api, _) = serve(vec![(404, "<html>")]);
        assert!(api.check_external_refs().unwrap_err().to_string().starts_with(OLD_BUDGET));
        let (api, _) = serve(vec![(401, "{}")]);
        assert!(api.check_external_refs().unwrap_err().to_string().contains("401"));
    }
}

//! Клиент REST API Семейной бухгалтерии (`Authorization: Bearer fbt_…`).

use anyhow::{Context, Result, anyhow, bail};
use chrono::NaiveDate;
use reqwest::{Method, blocking::Client};
use serde_json::{Value, json};
use std::time::Duration;

use crate::{
    config::{Category, Config},
    dedupe::Existing,
    keychain,
};

pub struct Api {
    http: Client,
    base: String,
    token: String,
}

/// Результат создания операции.
pub enum CreateError {
    /// сеть, сервер, лимит запросов, токен: повторим при следующем запуске
    Transient(anyhow::Error),
    /// бухгалтерия отклонила именно эту операцию (нет счёта, категория не того типа): повтор не поможет
    Rejected(String),
}

pub struct AccountInfo {
    pub currency: String,
    pub archived: bool,
}

impl Api {
    pub fn new(cfg: &Config) -> Result<Self> {
        let token = keychain::get(&cfg.keychain_service)?;
        if !token.starts_with("fbt_") {
            bail!("Это не похоже на API-токен: он начинается с fbt_. Создайте его в «Профиль → Безопасность» и выполните set-token");
        }
        Ok(Self {
            http: Client::builder().timeout(Duration::from_secs(20)).build()?,
            base: cfg.api_url.trim_end_matches('/').to_string(),
            token,
        })
    }

    fn call(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<&Value>) -> Result<(u16, String)> {
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
            401 => Some(anyhow!("{path}: 401 токен недействителен, истёк или отозван. Создайте новый и выполните set-token")),
            403 => Some(anyhow!(
                "{path}: 403 у токена нет нужного права ({body}). Для `run` достаточно transactions:write, для `init` нужен write: \
                 выдайте его на время настройки или создайте категории в приложении вручную"
            )),
            429 => Some(anyhow!("{path}: 429 слишком много запросов, повторим позже")),
            s if s >= 500 => Some(anyhow!("{path}: {s} ошибка сервера: {body}")),
            _ => None,
        }
    }

    fn get_json(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let (st, body) = self.call(Method::GET, path, query, None)?;
        if let Some(e) = Self::access_error(path, st, &body) {
            return Err(e);
        }
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

    /// Операции по счёту за период (включая переводы с этого и на этот счёт).
    pub fn transactions_around(&self, account: &str, from: NaiveDate, to: NaiveDate) -> Result<Vec<Existing>> {
        let q = [("from", from.to_string()), ("to", to.to_string()), ("account_code", account.to_string())];
        Ok(self.get_all("/api/transactions", &q)?.iter().filter_map(Existing::from_json).collect())
    }

    /// Создаёт операцию, возвращает её id.
    pub fn create_transaction(&self, payload: &Value) -> Result<String, CreateError> {
        let path = "/api/transactions";
        let (st, body) = self.call(Method::POST, path, &[], Some(payload)).map_err(CreateError::Transient)?;
        if let Some(e) = Self::access_error(path, st, &body) {
            return Err(CreateError::Transient(e));
        }
        match st {
            200 | 201 => {
                let v: Value = serde_json::from_str(&body).map_err(|e| CreateError::Transient(e.into()))?;
                Ok(v["id"].as_str().unwrap_or("?").to_string())
            }
            408 => Err(CreateError::Transient(anyhow!("{path}: 408 таймаут"))),
            _ => Err(CreateError::Rejected(format!("{st} {body}"))),
        }
    }

    pub fn account_info(&self, code: &str) -> Result<Option<AccountInfo>> {
        let path = format!("/api/accounts/{code}");
        let (st, body) = self.call(Method::GET, &path, &[], None)?;
        if let Some(e) = Self::access_error(&path, st, &body) {
            return Err(e);
        }
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
            403 => bail!("категория {code} не создана: у токена нет права записи в категории. Создайте её в приложении (тип {kind}) или выдайте токену право write"),
            _ => bail!("категория {code}: {st} {body}"),
        }
        Ok(())
    }
}

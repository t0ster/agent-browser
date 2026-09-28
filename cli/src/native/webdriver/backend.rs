use async_trait::async_trait;
use serde_json::Value;

/// Abstract backend for browser automation. CDP (Chromium) and WebDriver
/// (Safari/iOS) share this interface so actions.rs can remain backend-agnostic
/// in the future.
#[async_trait]
pub trait BrowserBackend: Send + Sync {
    async fn navigate(&self, url: &str) -> Result<(), String>;
    async fn get_url(&self) -> Result<String, String>;
    async fn get_title(&self) -> Result<String, String>;
    async fn get_content(&self) -> Result<String, String>;
    async fn evaluate(&self, script: &str) -> Result<Value, String>;
    async fn screenshot(&self) -> Result<String, String>;
    async fn click(&self, selector: &str) -> Result<(), String>;
    async fn fill(&self, selector: &str, value: &str) -> Result<(), String>;
    async fn close(&mut self) -> Result<(), String>;
    async fn back(&self) -> Result<(), String>;
    async fn forward(&self) -> Result<(), String>;
    async fn reload(&self) -> Result<(), String>;
    async fn get_cookies(&self) -> Result<Value, String>;
    fn backend_type(&self) -> &str;

    fn supports(&self, feature: &str) -> bool {
        match feature {
            "navigate" | "evaluate" | "screenshot" | "click" | "fill" => true,
            "screencast" | "tracing" | "network_intercept" | "cdp" => self.backend_type() == "cdp",
            _ => false,
        }
    }

    fn unsupported_error(&self, action: &str) -> String {
        format!(
            "Action '{}' is not supported on the {} backend",
            action,
            self.backend_type()
        )
    }
}

/// WebDriver implementation of BrowserBackend
pub struct WebDriverBackend {
    client: super::client::WebDriverClient,
}

impl WebDriverBackend {
    pub fn new(client: super::client::WebDriverClient) -> Self {
        Self { client }
    }

    /// Ref actions use the session's element identity, never a CSS re-lookup.
    pub async fn click_ref(&self, element_id: &str) -> Result<(), String> {
        self.client.click_element(element_id).await
    }

    pub async fn fill_ref(&self, element_id: &str, value: &str) -> Result<(), String> {
        self.client.clear_element(element_id).await?;
        self.client.send_keys(element_id, value).await
    }

    pub async fn element_matches(&self, element_id: &str, state: &str) -> Result<bool, String> {
        let condition = match state {
            "attached" => "e.isConnected",
            "detached" => "!e.isConnected",
            "visible" => "e.isConnected && !!e.getClientRects().length && getComputedStyle(e).visibility !== 'hidden'",
            "hidden" => "!e.isConnected || !e.getClientRects().length || getComputedStyle(e).visibility === 'hidden'",
            _ => return Err(format!("Unsupported selector state {state}")),
        };
        let result = self
            .client
            .execute_script(
                &format!("const e = arguments[0]; return {condition};"),
                vec![serde_json::json!({"element-6066-11e4-a52e-4f735466cecf":element_id})],
            )
            .await;
        match result {
            Ok(value) => value
                .as_bool()
                .ok_or("Invalid WebDriver element-state response".into()),
            Err(error)
                if error.starts_with("WebDriver error (stale element reference):")
                    && matches!(state, "hidden" | "detached") =>
            {
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }
}

#[async_trait]
impl BrowserBackend for WebDriverBackend {
    async fn navigate(&self, url: &str) -> Result<(), String> {
        self.client.navigate(url).await
    }

    async fn get_url(&self) -> Result<String, String> {
        self.client.get_url().await
    }

    async fn get_title(&self) -> Result<String, String> {
        self.client.get_title().await
    }

    async fn get_content(&self) -> Result<String, String> {
        self.client.get_page_source().await
    }

    async fn evaluate(&self, script: &str) -> Result<Value, String> {
        self.client.execute_script(script, vec![]).await
    }

    async fn screenshot(&self) -> Result<String, String> {
        self.client.screenshot().await
    }

    async fn click(&self, selector: &str) -> Result<(), String> {
        let element_id = self.client.find_element("css selector", selector).await?;
        self.client.click_element(&element_id).await
    }

    async fn fill(&self, selector: &str, value: &str) -> Result<(), String> {
        let element_id = self.client.find_element("css selector", selector).await?;
        self.client.clear_element(&element_id).await?;
        self.client.send_keys(&element_id, value).await
    }

    async fn close(&mut self) -> Result<(), String> {
        self.client.delete_session().await
    }

    async fn back(&self) -> Result<(), String> {
        self.client.back().await
    }

    async fn forward(&self) -> Result<(), String> {
        self.client.forward().await
    }

    async fn reload(&self) -> Result<(), String> {
        self.client.refresh().await
    }

    async fn get_cookies(&self) -> Result<Value, String> {
        self.client.get_cookies().await
    }

    fn backend_type(&self) -> &str {
        "webdriver"
    }
}

/// CDP-backed backend constants for unsupported actions on WebDriver
pub const WEBDRIVER_UNSUPPORTED_ACTIONS: &[&str] = &[
    // axe-core frame injection currently relies on CDP execution contexts.
    "a11y",
    "screencast_start",
    "screencast_stop",
    "trace_start",
    "trace_stop",
    "profiler_start",
    "profiler_stop",
    "route",
    "unroute",
    "expose",
    "addscript",
    "addinitscript",
    "network",
    "har_start",
    "har_stop",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unsupported_actions() {
        assert!(WEBDRIVER_UNSUPPORTED_ACTIONS.contains(&"a11y"));
        assert!(WEBDRIVER_UNSUPPORTED_ACTIONS.contains(&"screencast_start"));
        assert!(WEBDRIVER_UNSUPPORTED_ACTIONS.contains(&"trace_start"));
        assert!(!WEBDRIVER_UNSUPPORTED_ACTIONS.contains(&"navigate"));
    }
}

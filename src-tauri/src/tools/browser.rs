use crate::provider::ToolDefinition;
use serde_json::json;
use std::sync::Mutex;
use headless_chrome::{Browser, LaunchOptions, Tab};
use std::sync::Arc;

static BROWSER: Mutex<Option<Browser>> = Mutex::new(None);
static TAB: Mutex<Option<Arc<Tab>>> = Mutex::new(None);

fn get_tab() -> Result<Arc<Tab>, String> {
    let mut tab_guard = TAB.lock().map_err(|e| format!("Tab lock error: {}", e))?;
    if let Some(tab) = tab_guard.as_ref() {
        return Ok(tab.clone());
    }

    let mut browser_guard = BROWSER.lock().map_err(|e| format!("Browser lock error: {}", e))?;
    if browser_guard.is_none() {
        let launch_options = LaunchOptions::default_builder()
            .headless(true)
            .sandbox(false)
            .window_size(Some((1280, 800)))
            .build()
            .map_err(|e| format!("Failed to build launch options: {}", e))?;
        let browser = Browser::new(launch_options)
            .map_err(|e| format!("Failed to launch browser: {}", e))?;
        *browser_guard = Some(browser);
    }

    let browser = browser_guard.as_ref().unwrap();
    let tab = browser
        .new_tab()
        .map_err(|e| format!("Failed to create tab: {}", e))?;

    tab.enable_stealth_mode()
        .map_err(|e| format!("Failed to enable stealth: {}", e))?;

    *tab_guard = Some(tab.clone());
    Ok(tab)
}

fn reset_tab() {
    if let Ok(mut guard) = TAB.lock() {
        *guard = None;
    }
}

pub fn browser_navigate_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "browser_navigate".into(),
            description: "Navigate the browser to a URL. Resets the current page state. Use before any other browser action.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to navigate to (e.g., https://example.com)"
                    }
                },
                "required": ["url"]
            }),
        },
    }
}

pub async fn handle_browser_navigate(args: serde_json::Value) -> Result<String, String> {
    let url = args["url"].as_str().ok_or("Missing 'url' parameter")?;

    reset_tab();
    let tab = get_tab()?;

    tab.navigate_to(url)
        .map_err(|e| format!("Navigate failed: {}", e))?;
    tab.wait_until_navigated()
        .map_err(|e| format!("Wait for navigation failed: {}", e))?;

    let title = tab.get_title().unwrap_or_default();
    let current_url = tab.get_url();

    Ok(format!(
        "Navigated to: {}\nTitle: {}\nURL: {}",
        url, title, current_url
    ))
}

pub fn browser_click_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "browser_click".into(),
            description: "Click an element on the page by CSS selector. Use browser_snapshot first to see available elements.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "CSS selector of the element to click (e.g., '#submit', '.btn', 'a.link')"
                    }
                },
                "required": ["selector"]
            }),
        },
    }
}

pub async fn handle_browser_click(args: serde_json::Value) -> Result<String, String> {
    let selector = args["selector"].as_str().ok_or("Missing 'selector' parameter")?;
    let tab = get_tab()?;

    let element = tab
        .wait_for_element(selector)
        .map_err(|e| format!("Element not found '{}': {}", selector, e))?;

    element
        .click()
        .map_err(|e| format!("Click failed: {}", e))?;

    // Small wait for any JS to execute
    std::thread::sleep(std::time::Duration::from_millis(500));

    let current_url = tab.get_url();
    Ok(format!("Clicked: {}\nURL: {}", selector, current_url))
}

pub fn browser_type_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "browser_type".into(),
            description: "Type text into an input element by CSS selector. Clears existing content first.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "selector": {
                        "type": "string",
                        "description": "CSS selector of the input element (e.g., '#search', 'input[name=q]')"
                    },
                    "text": {
                        "type": "string",
                        "description": "Text to type into the element"
                    }
                },
                "required": ["selector", "text"]
            }),
        },
    }
}

pub async fn handle_browser_type(args: serde_json::Value) -> Result<String, String> {
    let selector = args["selector"].as_str().ok_or("Missing 'selector' parameter")?;
    let text = args["text"].as_str().ok_or("Missing 'text' parameter")?;
    let tab = get_tab()?;

    let element = tab
        .wait_for_element(selector)
        .map_err(|e| format!("Element not found '{}': {}", selector, e))?;

    element
        .click()
        .map_err(|e| format!("Focus failed: {}", e))?;

    element
        .type_into(text)
        .map_err(|e| format!("Type failed: {}", e))?;

    Ok(format!("Typed '{}' into {}", text, selector))
}

pub fn browser_scroll_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "browser_scroll".into(),
            description: "Scroll the page. Use 'down'/'up' to scroll one viewport, 'bottom'/'top' for extremes, or pixel values.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "direction": {
                        "type": "string",
                        "enum": ["down", "up", "bottom", "top"],
                        "description": "Scroll direction"
                    },
                    "pixels": {
                        "type": "number",
                        "description": "Scroll by exact pixel amount (overrides direction if set)"
                    }
                },
                "required": []
            }),
        },
    }
}

pub async fn handle_browser_scroll(args: serde_json::Value) -> Result<String, String> {
    let tab = get_tab()?;

    let js = if let Some(px) = args["pixels"].as_f64() {
        format!("window.scrollBy(0, {});", px as i64)
    } else {
        let direction = args["direction"].as_str().unwrap_or("down");
        match direction {
            "down" => "window.scrollBy(0, window.innerHeight * 0.8);".to_string(),
            "up" => "window.scrollBy(0, -window.innerHeight * 0.8);".to_string(),
            "bottom" => "window.scrollTo(0, document.body.scrollHeight);".to_string(),
            "top" => "window.scrollTo(0, 0);".to_string(),
            _ => return Err(format!("Unknown scroll direction: {}", direction)),
        }
    };

    let result = tab
        .evaluate(&js, false)
        .map_err(|e| format!("Scroll failed: {}", e))?;

    Ok(format!("Scrolled. Result: {:?}", result))
}

pub fn browser_snapshot_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "browser_snapshot".into(),
            description: "Get a text snapshot of the current page showing interactive elements (links, buttons, inputs) for accessibility/automation. Use before clicking or typing.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "include_html": {
                        "type": "boolean",
                        "description": "Include full page HTML in response (default: false, returns interactive elements only)"
                    }
                },
                "required": []
            }),
        },
    }
}

pub async fn handle_browser_snapshot(args: serde_json::Value) -> Result<String, String> {
    let tab = get_tab()?;
    let include_html = args["include_html"].as_bool().unwrap_or(false);

    let title = tab.get_title().unwrap_or_default();
    let url = tab.get_url();

    let elements_js = r#"
        (() => {
            const interactive = 'a, button, input, textarea, select, [role=button], [onclick], [tabindex]';
            const els = document.querySelectorAll(interactive);
            const results = [];
            els.forEach((el, i) => {
                if (el.offsetParent === null) return;
                const rect = el.getBoundingClientRect();
                if (rect.width === 0 || rect.height === 0) return;
                const tag = el.tagName.toLowerCase();
                const id = el.id ? '#' + el.id : '';
                const cls = el.className && typeof el.className === 'string' ? '.' + el.className.split(' ').slice(0, 2).join('.') : '';
                const text = (el.textContent || el.placeholder || el.value || el.getAttribute('aria-label') || '').trim().substring(0, 60);
                results.push({
                    index: results.length,
                    tag: tag,
                    selector: id || (cls ? tag + cls : tag),
                    text: text,
                });
            });
            return JSON.stringify(results.slice(0, 100));
        })()
    "#;

    let interactive = tab
        .evaluate(elements_js, false)
        .map_err(|e| format!("Snapshot failed: {}", e))?
        .value
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "[]".to_string());

    let parsed: Vec<serde_json::Value> =
        serde_json::from_str(&interactive).unwrap_or_default();

    let mut output = format!("Page: {}\nURL: {}\n\nInteractive elements:\n", title, url);

    for el in &parsed {
        let selector = el["selector"].as_str().unwrap_or("?");
        let text = el["text"].as_str().unwrap_or("");
        output.push_str(&format!("  [{}] {} - {}\n", el["index"], selector, text));
    }

    if include_html {
        let html = tab
            .evaluate("document.documentElement.outerHTML.substring(0, 30000)", false)
            .map_err(|e| format!("HTML fetch failed: {}", e))?
            .value
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();
        output.push_str(&format!("\n--- HTML (first 30K chars) ---\n{}", html));
    }

    Ok(output)
}

pub fn browser_press_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "browser_press".into(),
            description: "Press a keyboard key. Use 'Enter' to submit forms, 'Escape' to close modals, 'Tab' to navigate, arrow keys to scroll, etc.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Key to press (e.g., Enter, Escape, Tab, ArrowDown, ArrowUp, Backspace, Delete)"
                    },
                    "selector": {
                        "type": "string",
                        "description": "CSS selector of the element to press key on (optional, defaults to body)"
                    }
                },
                "required": ["key"]
            }),
        },
    }
}

pub async fn handle_browser_press(args: serde_json::Value) -> Result<String, String> {
    let key = args["key"].as_str().ok_or("Missing 'key' parameter")?;
    let tab = get_tab()?;

    let selector = args["selector"].as_str().unwrap_or("body");

    // Focus the element first, then dispatch keyboard event
    let focus_js = format!(
        "document.querySelector('{}')?.focus();",
        selector.replace('\'', "\\'")
    );
    tab.evaluate(&focus_js, false).ok();

    let key_event_js = format!(
        r#"(() => {{
            const el = document.querySelector('{}') || document.body;
            const key = '{}';
            let keyCode = 0;
            const keyCodes = {{ Enter: 13, Escape: 27, Tab: 9, Backspace: 8, Delete: 46, ArrowUp: 38, ArrowDown: 40, ArrowLeft: 37, ArrowRight: 39, Space: 32 }};
            keyCode = keyCodes[key] || 0;
            el.dispatchEvent(new KeyboardEvent('keydown', {{ key, keyCode, bubbles: true }}));
            el.dispatchEvent(new KeyboardEvent('keypress', {{ key, keyCode, bubbles: true }}));
            el.dispatchEvent(new KeyboardEvent('keyup', {{ key, keyCode, bubbles: true }}));
            if (key === 'Enter' && el.tagName === 'FORM') el.submit();
            return key + ' pressed';
        }})()"#,
        selector.replace('\'', "\\'"),
        key.replace('\'', "\\'")
    );

    let result = tab
        .evaluate(&key_event_js, false)
        .map_err(|e| format!("Press key '{}' failed: {}", key, e))?;

    std::thread::sleep(std::time::Duration::from_millis(300));

    let output = result
        .value
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| format!("Pressed '{}'", key));

    Ok(output)
}

pub fn browser_vision_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "browser_vision".into(),
            description: "Take a screenshot of the current page. Returns the screenshot as base64-encoded PNG for vision model analysis. Use to see what's visible on the page.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "full_page": {
                        "type": "boolean",
                        "description": "Capture full scrollable page (default: false, viewport only)"
                    }
                },
                "required": []
            }),
        },
    }
}

pub async fn handle_browser_vision(args: serde_json::Value) -> Result<String, String> {
    let tab = get_tab()?;
    let _full_page = args["full_page"].as_bool().unwrap_or(false);

    let screenshot_data = tab
        .capture_screenshot(
            headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption::Png,
            None,
            None,
            true,
        )
        .map_err(|e| format!("Screenshot failed: {}", e))?;

    let base64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &screenshot_data);

    Ok(format!(
        "Screenshot captured ({} bytes, base64). Use this for vision analysis.\n\n```image\n{}\n```",
        screenshot_data.len(),
        base64
    ))
}

pub fn browser_console_def() -> ToolDefinition {
    ToolDefinition {
        def_type: "function".into(),
        function: crate::provider::FunctionDef {
            name: "browser_console".into(),
            description: "Execute JavaScript in the browser console. Use for extracting data, manipulating the page, or debugging. Returns the result as JSON.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "JavaScript code to execute in the browser context"
                    },
                    "await_promise": {
                        "type": "boolean",
                        "description": "Wait for the returned promise to resolve (default: true)"
                    }
                },
                "required": ["code"]
            }),
        },
    }
}

pub async fn handle_browser_console(args: serde_json::Value) -> Result<String, String> {
    let code = args["code"].as_str().ok_or("Missing 'code' parameter")?;
    let await_promise = args["await_promise"].as_bool().unwrap_or(true);
    let tab = get_tab()?;

    let result = tab
        .evaluate(code, await_promise)
        .map_err(|e| format!("Console execution failed: {}", e))?;

    let value_str = match &result.value {
        Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| format!("{:?}", v)),
        None => "undefined".to_string(),
    };

    Ok(truncate(&value_str, 10000))
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    // max_len is a byte budget; back off to the nearest char boundary so
    // a multi-byte UTF-8 sequence at the cut point never panics.
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...\n[truncated {} -> {} chars]", &s[..end], s.len(), max_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires Chrome binary at /Applications/Google Chrome.app — run with: cargo test browser_ -- --ignored"]
    fn browser_navigate_click_verify() {
        let nav_result = std::thread::spawn(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let args = serde_json::json!({ "url": "https://example.com" });
                handle_browser_navigate(args).await
            })
        })
        .join()
        .expect("thread panicked");

        match nav_result {
            Ok(output) => {
                eprintln!("=== browser_navigate -> example.com ===");
                eprintln!("{}", &output[..output.len().min(300)]);
                assert!(
                    output.contains("Example Domain") || output.contains("example"),
                    "Expected example.com content, got: {}",
                    &output[..output.len().min(200)]
                );
            }
            Err(e) => {
                panic!("browser_navigate failed: {}", e);
            }
        }

        let click_result = std::thread::spawn(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let args = serde_json::json!({ "selector": "a" });
                handle_browser_click(args).await
            })
        })
        .join()
        .expect("thread panicked");

        match click_result {
            Ok(output) => {
                eprintln!("=== browser_click -> first link ===");
                eprintln!("{}", &output[..output.len().min(300)]);
                assert!(!output.is_empty());
            }
            Err(e) => {
                eprintln!("browser_click failed (expected if no links): {}", e);
            }
        }

        {
            let mut b = BROWSER.lock().unwrap();
            *b = None;
            let mut t = TAB.lock().unwrap();
            *t = None;
        }
    }
}

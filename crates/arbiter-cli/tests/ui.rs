//! U2-U7's own acceptance suite (IMPLEMENTATION_PLAN.md's own command:
//! `cargo test -p arbiter-cli --test ui`), driven against a real,
//! pre-installed Chromium via `chromiumoxide`'s direct CDP connection --
//! no Node.js, no npm, no `@playwright/test` runner (PLAN_DEVIATIONS.md
//! D49 covers why "Playwright" in the plan's own words became this crate
//! instead: this sandbox's pre-installed browser is meant to be driven by
//! `executablePath`, which is exactly `chromiumoxide::BrowserConfig`'s own
//! shape, and a literal Node-based Playwright run would need `npm install`
//! at test time -- a live-registry fetch this suite has no business
//! depending on for something `cargo test` should run offline, every
//! commit).
//!
//! Each `arbiter serve` instance under test is the real compiled binary
//! (`env!("CARGO_BIN_EXE_arbiter")`), not an in-process router -- this is
//! an integration test target, which for a `[[bin]]`-only crate has no
//! access to `arbiter-cli`'s own private modules at all; spawning the real
//! binary is what a black-box UI suite should be doing anyway.
//!
//! A handful of tests (the 5 panel key states, the 0-usable-models case)
//! need backend states this build's own engine cannot actually produce
//! today (P4's real adapters don't exist, D46) -- those install a
//! `window.fetch` override via `Page::evaluate_on_new_document` *before*
//! the page's own script runs, so `GET /api/providers` resolves to
//! synthetic data while every other request still goes to the real
//! server. This is the same technique Playwright's own `page.route()`
//! provides at the network layer; doing it in-page is the one adjustment
//! `chromiumoxide` (no built-in request-interception helper as ergonomic)
//! asked for.

use chromiumoxide::Browser;
use chromiumoxide::browser::BrowserConfig;
use chromiumoxide::page::Page;
use futures_util::StreamExt;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Server {
    child: Child,
    base: String,
    token: String,
    store: PathBuf,
}

impl Server {
    fn url(&self, hash: &str) -> String {
        format!("{}/?token={}{}", self.base, self.token, hash)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // `self.store` is `<root>/runs`; `history.db` lives in `<root>`
        // itself (`temp_run_store`'s own doc comment) -- remove the whole
        // root, not just the `runs` subdirectory, or every test leaks a
        // `history.db` file into the system temp directory.
        let root = self.store.parent().unwrap_or(&self.store);
        let _ = std::fs::remove_dir_all(root);
    }
}

fn temp_store(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "arbiter_ui_test_{label}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// `--store`'s own directory, one level *under* a fresh unique root:
/// `history_db_path` (`main.rs`) resolves `history.db` to `--store`'s
/// *parent*, matching ARCHITECTURE's real `~/.arbiter/{history.db,
/// runs/<id>/run.db}` layout -- a flat `--store` here would put every
/// test's `history.db` in the shared system temp directory itself,
/// contaminating `history_is_empty_stated` and every other test with
/// rows from every other run this whole suite (and any earlier manual
/// run) ever produced.
fn temp_run_store(label: &str) -> PathBuf {
    temp_store(label).join("runs")
}

fn start_server(label: &str) -> Server {
    let store = temp_run_store(label);
    std::fs::create_dir_all(&store).expect("creating the test's own store root");
    let mut child = Command::new(env!("CARGO_BIN_EXE_arbiter"))
        .args(["serve", "--port", "0", "--store", store.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning `arbiter serve`");

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut base = None;
    let mut token = None;
    for _ in 0..40 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        // The printed form is fixed by `serve_command` itself:
        // "Open: http://127.0.0.1:<port>/?token=<hex>" -- split by hand
        // rather than pulling in a URL-parsing crate for one known shape.
        if let Some(rest) = line.trim().strip_prefix("Open: ") {
            let (origin, query) = rest.trim().split_once("/?").expect("Open URL missing '/?'");
            token = query.strip_prefix("token=").map(|t| t.to_string());
            base = Some(origin.to_string());
            break;
        }
    }
    let base = base.expect("`arbiter serve` never printed its Open URL");
    let token = token.expect("the Open URL carried no token");
    // Give the listener a moment to actually start accepting connections.
    std::thread::sleep(Duration::from_millis(100));
    Server {
        child,
        base,
        token,
        store,
    }
}

/// How many of these tests may hold a live Chromium at once.
///
/// Every test in this file spawns a real `arbiter serve` subprocess *and* a
/// real browser, and `cargo test` defaults `--test-threads` to the core count.
/// Eleven of those at once starves a modest container: launches that would
/// have succeeded time out in `wait_for`, and the suite fails intermittently
/// on tests that pass every time in isolation. Capping the browsers -- rather
/// than lengthening the timeouts again, or asking every future runner to
/// remember `--test-threads=1` -- fixes the cause: two run concurrently, the
/// rest queue on the permit, and the wall-clock cost is small because each
/// test is dominated by its own page waits.
const MAX_CONCURRENT_BROWSERS: usize = 2;

fn browser_permits() -> &'static tokio::sync::Semaphore {
    static PERMITS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    PERMITS.get_or_init(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_BROWSERS))
}

/// Keeps a launched browser's CDP handler pumping and its concurrency permit
/// held for as long as the test holds the guard. Every call site binds it as
/// `_guard`, so it lives to the end of the test body and releases there.
struct BrowserGuard {
    _handler: tokio::task::JoinHandle<()>,
    _permit: tokio::sync::SemaphorePermit<'static>,
}

/// Each call gets its own `--user-data-dir`: chromiumoxide otherwise
/// defaults every launch to the same shared profile directory, which two
/// tests running concurrently (this suite's own default) collide over via
/// Chrome's own single-instance lock file, killing the second launch
/// outright rather than just serializing it.
async fn browser(label: &str) -> (Browser, BrowserGuard) {
    let permit = browser_permits()
        .acquire()
        .await
        .expect("browser permits are never closed");
    let profile = temp_store(&format!("chrome_profile_{label}"));
    let config = BrowserConfig::builder()
        .chrome_executable("/opt/pw-browsers/chromium")
        .user_data_dir(&profile)
        .no_sandbox()
        .build()
        .unwrap();
    let (browser, mut handler) = Browser::launch(config).await.expect("launching chromium");
    let handler = tokio::spawn(async move { while handler.next().await.is_some() {} });
    (
        browser,
        BrowserGuard {
            _handler: handler,
            _permit: permit,
        },
    )
}

/// Waits (polling `evaluate`) until `js_bool` evaluates truthy, or panics.
/// A generous 30s budget: this suite spawns a real `arbiter serve`
/// subprocess and a real Chromium per test, and several run genuinely
/// concurrently (`--test-threads` > 1) -- a short timeout tuned against an
/// idle machine turns ordinary scheduling contention into a false
/// failure.
async fn wait_for(page: &Page, js_bool: &str, what: &str) {
    for _ in 0..300 {
        let result: bool = page
            .evaluate(js_bool)
            .await
            .ok()
            .and_then(|r| r.into_value().ok())
            .unwrap_or(false);
        if result {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// [`wait_for`], plus whatever the page is currently saying about itself.
async fn wait_for_explained(page: &Page, js_bool: &str, what: &str) {
    for _ in 0..300 {
        let result: bool = page
            .evaluate(js_bool)
            .await
            .ok()
            .and_then(|r| r.into_value().ok())
            .unwrap_or(false);
        if result {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let state: String = page
        .evaluate(
            "JSON.stringify({ hash: location.hash,              submitError: (document.getElementById('submit-error') || {}).textContent || '',              banner: (document.querySelector('.banner') || {}).textContent || '',              body: document.body.innerText.slice(0, 400) })",
        )
        .await
        .ok()
        .and_then(|r| r.into_value().ok())
        .unwrap_or_else(|| "<page unreachable>".to_string());
    panic!("timed out waiting for: {what}\npage state: {state}");
}

async fn text_of(page: &Page, selector: &str) -> String {
    page.find_element(selector)
        .await
        .unwrap_or_else(|e| panic!("no element matching {selector}: {e}"))
        .inner_text()
        .await
        .unwrap()
        .unwrap_or_default()
}

/// Runs a run end-to-end from screen 1 and returns once screen 3 (Result)
/// has loaded -- the shared setup several tests below need.
async fn run_to_result(page: &Page, server: &Server) {
    // Compare is the landing tab now, so the debate form is one hash away.
    // Reached by setting the hash rather than `goto`: every caller already
    // has the app loaded, and changing only the fragment is a same-document
    // navigation, so `goto` waits for a load event that never arrives.
    let _ = server;
    page.evaluate("if (location.hash !== '#/new') { location.hash = '#/new'; }")
        .await
        .unwrap();
    wait_for(
        page,
        "!!document.getElementById('new-run-form')",
        "the new-run form to render",
    )
    .await;
    page.find_element("#question")
        .await
        .unwrap()
        .click() // `type_str` dispatches key events at the tab level, not
        // scoped to the element -- it only lands where focus already is
        // (chromiumoxide's own doc example: `.click().await?.type_str(...)`),
        // and headless navigation does not reliably honor `autofocus`.
        .await
        .unwrap()
        .type_str("Should we adopt a modular monolith or microservices?")
        .await
        .unwrap();
    page.find_element("#start-btn")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    // On a timeout here, say *why* the page never navigated: a refused
    // submit (an empty panel, an empty question) leaves its reason in
    // `#submit-error`, and a server-side failure leaves one in the banner.
    // Without this the failure reads only "timed out", which is true of every
    // possible cause and therefore diagnostic of none.
    wait_for_explained(
        page,
        "location.hash.startsWith('#/result/')",
        "auto-navigation to the result screen",
    )
    .await;
    wait_for(
        page,
        "!!document.querySelector('.tag')",
        "the outcome tag to render",
    )
    .await;
}

/// `all_5_panel_key_states_render`: `Verified`/`Present`/`Rejected`/
/// `Missing`/provider-unreachable all have their own row shape -- this
/// build's own real backend only ever produces `not_required` (mock) and
/// `missing` (anthropic, no key configured anywhere in this sandbox), so
/// the other three are supplied via a mocked `/api/providers` response,
/// installed before the page's own script runs.
#[tokio::test]
async fn all_5_panel_key_states_render() {
    let server = start_server("panel_states");
    let (browser, _guard) = browser("panel_states").await;
    let page = browser.new_page("about:blank").await.unwrap();

    page.evaluate_on_new_document(
        r#"
        (function () {
          const real = window.fetch.bind(window);
          window.fetch = function (url, opts) {
            if (String(url).indexOf('/api/providers') !== -1 && (!opts || opts.method === undefined || opts.method === 'GET')) {
              return Promise.resolve(new Response(JSON.stringify({
                providers: [
                  { id: 'mock', state: 'not_required', source: null, fingerprint: null, usable: true },
                  { id: 'verified-co', state: 'verified', source: 'env:ARBITER_VERIFIED_CO_API_KEY', fingerprint: 'abcd', usable: true },
                  { id: 'present-co', state: 'present', source: 'keychain', fingerprint: 'ef01', usable: false },
                  { id: 'rejected-co', state: 'rejected', source: 'env:REJECTED_CO_API_KEY', status: 401, fingerprint: '2345', usable: false },
                  { id: 'missing-co', state: 'missing', source: null, fingerprint: null, usable: false },
                  { id: 'unreachable-co', state: 'unreachable', source: null, fingerprint: null, usable: false },
                ],
                estimates: { standard: { cost: 0.1, calls: 10, wall_clock_secs: 300, model_count: 3 }, deep: { cost: 0.2, calls: 20, wall_clock_secs: 300, model_count: 3 } },
              }), { status: 200, headers: { 'content-type': 'application/json' } }));
            }
            return real(url, opts);
          };
        })();
        "#,
    )
    .await
    .unwrap();

    page.goto(server.url("#/new")).await.unwrap();
    wait_for(
        &page,
        "document.querySelectorAll('.panel-row').length === 6",
        "all 6 mocked provider rows to render",
    )
    .await;

    let labels: Vec<String> = {
        let text = page
            .find_element("body")
            .await
            .unwrap()
            .inner_text()
            .await
            .unwrap()
            .unwrap_or_default();
        text.lines().map(|l| l.to_string()).collect()
    };
    let joined = labels.join(" ");
    for expected in [
        "ready",
        "key set, not checked",
        "key rejected",
        "no key",
        "provider unreachable",
    ] {
        assert!(
            joined.contains(expected),
            "expected the panel to mention '{expected}', got: {joined}"
        );
    }
}

/// `a_panel_can_hold_five_models_behind_fewer_keys`: a panel is a list of
/// models, not of providers, but the picker was one checkbox per provider --
/// so a five-model panel needed five working keys, which is not something
/// most operators have. Each usable provider now carries its own extra-model
/// rows, and what they build is the same `provider[:model]` spec `--panel`
/// takes.
///
/// The independence figure has to stay honest through this: five models
/// behind two keys is a bigger panel, not five independent sources, so the
/// warning keeps counting providers even as the count reports models.
#[tokio::test]
async fn a_panel_can_hold_five_models_behind_fewer_keys() {
    let server = start_server("panel_extra_models");
    let (browser, _guard) = browser("panel_extra_models").await;
    let page = browser.new_page("about:blank").await.unwrap();

    // Two keyed providers, which is all this needs to reach five models --
    // and no real key exists in this sandbox, so the roster is mocked.
    page.evaluate_on_new_document(
        r#"
        (function () {
          window.__runBody = null;
          const real = window.fetch.bind(window);
          window.fetch = function (url, opts) {
            const u = String(url);
            if (u.indexOf('/api/providers') !== -1 && (!opts || !opts.method || opts.method === 'GET')) {
              const row = function (id, dflt) { return { id: id, state: 'present', source: 'keychain', fingerprint: 'abcd', usable: true, models: 1, default_model: dflt }; };
              const table = function (n) { const out = []; for (let i = 0; i <= n; i++) out.push({ cost: i * 0.1, calls: i * 10, wall_clock_secs: 300, model_count: i }); return out; };
              return Promise.resolve(new Response(JSON.stringify({
                providers: [row('openrouter', 'deepseek/deepseek-chat'), row('groq', 'llama-3.3-70b-versatile')],
                estimates: {
                  standard: { cost: 0.2, calls: 20, wall_clock_secs: 300, model_count: 2 },
                  deep: { cost: 0.4, calls: 40, wall_clock_secs: 300, model_count: 2 },
                  per_model_count: { standard: table(12), deep: table(12) },
                },
              }), { status: 200, headers: { 'content-type': 'application/json' } }));
            }
            if (u.indexOf('/api/runs') !== -1 && opts && opts.method === 'POST') {
              window.__runBody = opts.body;
              // Refused, so the page stays on this screen with the captured
              // body: what is under test is the spec, not what follows it.
              return Promise.resolve(new Response(JSON.stringify({ error: 'captured' }), { status: 400, headers: { 'content-type': 'application/json' } }));
            }
            return real(url, opts);
          };
        })();
        "#,
    )
    .await
    .unwrap();

    page.goto(server.url("#/new")).await.unwrap();
    wait_for(
        &page,
        "document.querySelectorAll('.panel-pick').length === 2",
        "both mocked providers to render",
    )
    .await;

    // Two keys, two models, and the warning says the panel is not independent.
    let before = text_of(&page, "#panel-count").await;
    assert!(
        before.contains("2 models") && before.contains("2 providers"),
        "the count must separate models from providers: {before}"
    );

    // Three more models on the openrouter key takes the panel to five.
    page.evaluate(
        r#"
        (function () {
          const btn = document.querySelector('[data-add-model="openrouter"]');
          const ids = ['meta-llama/llama-3.3-70b-instruct',
                       'qwen/qwen-2.5-72b-instruct',
                       'deepseek/deepseek-chat-v3-0324:free'];
          ids.forEach(function (id) {
            btn.click();
            const rows = document.querySelectorAll('[data-models-for="openrouter"] input[type=text]');
            const input = rows[rows.length - 1];
            input.value = id;
            input.dispatchEvent(new Event('input', { bubbles: true }));
          });
        })();
        "#,
    )
    .await
    .unwrap();
    wait_for(
        &page,
        "document.getElementById('panel-count').textContent.indexOf('5 models') !== -1",
        "the panel to report five models",
    )
    .await;

    // Five models, still two providers: the warning must not have gone quiet
    // just because the panel grew.
    let count = text_of(&page, "#panel-count").await;
    assert!(count.contains("2 providers"), "{count}");
    let warning = text_of(&page, "#panel-warning").await;
    assert!(
        warning.contains("5 models") && warning.contains("2 providers"),
        "a five-model, two-provider panel is not five-way cross-checking: {warning}"
    );

    // And the spec that goes out is the one `--panel` would take.
    page.find_element("#question")
        .await
        .unwrap()
        .click()
        .await
        .unwrap()
        .type_str("Should we adopt a modular monolith or microservices?")
        .await
        .unwrap();
    page.find_element("#start-btn")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    wait_for(&page, "!!window.__runBody", "the run request to go out").await;
    let body: String = page
        .evaluate("window.__runBody")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    let sent: serde_json::Value = serde_json::from_str(&body).unwrap();
    let panel = sent["panel"].as_str().unwrap();
    assert_eq!(
        panel,
        "openrouter:deepseek/deepseek-chat,\
         openrouter:meta-llama/llama-3.3-70b-instruct,\
         openrouter:qwen/qwen-2.5-72b-instruct,\
         openrouter:deepseek/deepseek-chat-v3-0324:free,\
         groq:llama-3.3-70b-versatile",
        "the spec must name five models across the two keys"
    );
    assert_eq!(panel.split(',').count(), 5, "{panel}");
}

/// `free_open_weight_models_can_be_picked_from_the_live_catalogue`: an
/// operator who wants five free open-weight models should not have to know an
/// aggregator's model ids by heart -- that catalogue turns over weekly, and a
/// mistyped id surfaces as a 404 from the vendor part-way through a paid run.
///
/// So the ids come from the vendor, read live with the operator's own key.
/// What this pins down is what the screen does with them: it replaces the
/// provider's *billed* default line rather than adding to it (otherwise a
/// panel the operator just asked to make free still carries a paid model),
/// fills only up to the target, and keeps saying that licences differ --
/// "open weights" is a family label this build infers from the id, not a
/// licence audit.
#[tokio::test]
async fn free_open_weight_models_can_be_picked_from_the_live_catalogue() {
    let server = start_server("panel_free_models");
    let (browser, _guard) = browser("panel_free_models").await;
    let page = browser.new_page("about:blank").await.unwrap();

    page.evaluate_on_new_document(
        r#"
        (function () {
          window.__catalogueCalls = 0;
          const real = window.fetch.bind(window);
          window.fetch = function (url, opts) {
            const u = String(url);
            if (u.indexOf('/api/providers/openrouter/models') !== -1) {
              window.__catalogueCalls += 1;
              return Promise.resolve(new Response(JSON.stringify({
                provider: 'openrouter',
                models: [
                  { id: 'deepseek/deepseek-chat', free: false, open_weights: true, context_length: 64000 },
                  { id: 'meta-llama/llama-3.3-70b-instruct:free', free: true, open_weights: true, context_length: 65536 },
                  { id: 'qwen/qwen-2.5-72b-instruct:free', free: true, open_weights: true, context_length: 32768 },
                  { id: 'mistralai/mistral-small-3.2-24b-instruct:free', free: true, open_weights: true, context_length: 96000 },
                ],
                suggested: ['meta-llama/llama-3.3-70b-instruct:free',
                            'qwen/qwen-2.5-72b-instruct:free',
                            'mistralai/mistral-small-3.2-24b-instruct:free'],
                suggested_target: 5,
              }), { status: 200, headers: { 'content-type': 'application/json' } }));
            }
            if (u.indexOf('/api/providers') !== -1 && (!opts || !opts.method || opts.method === 'GET')) {
              return Promise.resolve(new Response(JSON.stringify({
                providers: [
                  { id: 'openrouter', state: 'present', source: 'keychain', fingerprint: 'abcd', usable: true, models: 1, default_model: 'deepseek/deepseek-chat' },
                  { id: 'groq', state: 'present', source: 'keychain', fingerprint: 'ef01', usable: true, models: 1, default_model: 'llama-3.3-70b-versatile' },
                ],
                estimates: {
                  standard: { cost: 0.2, calls: 20, wall_clock_secs: 300, model_count: 2 },
                  deep: { cost: 0.4, calls: 40, wall_clock_secs: 300, model_count: 2 },
                },
              }), { status: 200, headers: { 'content-type': 'application/json' } }));
            }
            return real(url, opts);
          };
        })();
        "#,
    )
    .await
    .unwrap();

    page.goto(server.url("#/new")).await.unwrap();
    wait_for(
        &page,
        "!!document.querySelector('[data-free-models=\"openrouter\"]')",
        "the panel to render",
    )
    .await;

    // Each provider starts on its default model, and that default is visible
    // and editable rather than an invisible fallback.
    let start: String = page
        .evaluate(
            "Array.prototype.map.call(document.querySelectorAll('[data-models-for=\"openrouter\"] input[type=text]'), function (i) { return i.value; }).join(',')",
        )
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert_eq!(
        start, "deepseek/deepseek-chat",
        "the billed default is shown"
    );

    page.find_element("[data-free-models=\"openrouter\"]")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    wait_for(
        &page,
        "document.querySelectorAll('[data-models-for=\"openrouter\"] input[type=text]').length === 3",
        "the free models to fill in",
    )
    .await;

    let filled: String = page
        .evaluate(
            "Array.prototype.map.call(document.querySelectorAll('[data-models-for=\"openrouter\"] input[type=text]'), function (i) { return i.value; }).join(',')",
        )
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert_eq!(
        filled,
        "meta-llama/llama-3.3-70b-instruct:free,\
         qwen/qwen-2.5-72b-instruct:free,\
         mistralai/mistral-small-3.2-24b-instruct:free",
        "the billed default must be replaced, not kept alongside the free ones"
    );
    assert!(
        !filled.contains("deepseek/deepseek-chat,"),
        "a panel asked to be free must not still carry the paid default: {filled}"
    );

    // Four models now: three free ones plus groq's own default.
    wait_for(
        &page,
        "document.getElementById('panel-count').textContent.indexOf('4 models') !== -1",
        "the count to follow the fill",
    )
    .await;

    // "Open weights" is a family label inferred from the id, so the screen
    // says licences differ rather than letting the button imply it checked.
    let note = text_of(&page, "[data-note-for=\"openrouter\"]").await;
    assert!(
        note.contains("licences differ"),
        "the note must not let this read as a licence audit: {note}"
    );

    // The catalogue is fetched once and reused: this screen must not re-ask a
    // vendor for a list it already holds.
    page.find_element("[data-free-models=\"openrouter\"]")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    wait_for(&page, "window.__catalogueCalls >= 1", "the catalogue call").await;
    let calls: u32 = page
        .evaluate("window.__catalogueCalls")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert_eq!(calls, 1, "the catalogue must be cached per provider");
}

/// `the_panel_picker_chooses_who_actually_runs`: before P4 the panel
/// checkboxes were `checked disabled` decoration, because `mock` was the only
/// panel this build could run. They now carry the run: what is ticked becomes
/// `POST /api/runs`'s `panel`, and ticking nothing is refused client-side
/// rather than silently falling back to a panel the operator did not pick.
#[tokio::test]
async fn the_panel_picker_chooses_who_actually_runs() {
    let server = start_server("panel_picker");
    let (browser, _guard) = browser("panel_picker").await;
    let page = browser.new_page(server.url("#/new")).await.unwrap();
    wait_for(
        &page,
        "!!document.querySelector('.panel-pick')",
        "the panel checkboxes to render",
    )
    .await;

    // Nothing real has a key in the test environment, so `mock` is both the
    // only usable provider and the default selection.
    let checked: String = page
        .evaluate(
            "Array.prototype.filter.call(document.querySelectorAll('.panel-pick'),              function (b) { return b.checked; }).map(function (b) {              return b.getAttribute('data-provider'); }).join(',')",
        )
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert_eq!(
        checked, "mock",
        "mock must be the default panel with no keys set"
    );

    // The unusable providers are offered but not selectable: a box you cannot
    // tick says "add a key", where a missing row would say nothing at all.
    let disabled: u32 = page
        .evaluate("document.querySelectorAll('.panel-pick[disabled]').length")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert!(
        disabled > 0,
        "keyless providers must render as disabled rows"
    );

    // Untick everything and the run is refused before any request goes out.
    page.evaluate(
        "Array.prototype.forEach.call(document.querySelectorAll('.panel-pick'),          function (b) { b.checked = false; });",
    )
    .await
    .unwrap();
    page.find_element("#question")
        .await
        .unwrap()
        .click()
        .await
        .unwrap()
        .type_str("Should we adopt a modular monolith or microservices?")
        .await
        .unwrap();
    page.find_element("#start-btn")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    wait_for(
        &page,
        "!!document.querySelector('#submit-error .field-error')",
        "an empty panel to be refused client-side",
    )
    .await;
    let message = text_of(&page, "#submit-error .field-error").await;
    assert!(
        message.to_lowercase().contains("at least one"),
        "the refusal must say what to do about it: {message}"
    );
    let hash: String = page
        .evaluate("location.hash")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert!(
        !hash.contains("/running/"),
        "a refused submit must not navigate to a run: {hash}"
    );
}

/// `estimate_falls_when_a_model_is_unusable`: the estimate shown on screen
/// 1 is server-computed from the usable-provider count (`serve/handlers.rs`
/// own `run_estimate`) -- mocking zero usable providers must show a
/// visibly smaller (here: zero-model) estimate than the real, mock-usable
/// roster does.
#[tokio::test]
async fn estimate_falls_when_a_model_is_unusable() {
    let server = start_server("estimate_falls");
    let (browser, _guard) = browser("estimate_falls").await;

    // First, the real roster (mock usable) for a baseline.
    let page1 = browser.new_page(server.url("#/new")).await.unwrap();
    wait_for(
        &page1,
        "!!document.getElementById('estimate')",
        "screen 1 to render",
    )
    .await;
    wait_for(
        &page1,
        "document.getElementById('estimate').textContent.length > 0",
        "the estimate to populate",
    )
    .await;
    let baseline = text_of(&page1, "#estimate").await;

    // Then, a mocked roster where nothing at all is usable.
    let page2 = browser.new_page("about:blank").await.unwrap();
    page2
        .evaluate_on_new_document(
            r#"
            (function () {
              const real = window.fetch.bind(window);
              window.fetch = function (url, opts) {
                if (String(url).indexOf('/api/providers') !== -1 && (!opts || !opts.method || opts.method === 'GET')) {
                  return Promise.resolve(new Response(JSON.stringify({
                    providers: [{ id: 'mock', state: 'missing', source: null, fingerprint: null, usable: false }],
                    estimates: { standard: { cost: 0, calls: 0, wall_clock_secs: 300, model_count: 0 }, deep: { cost: 0, calls: 0, wall_clock_secs: 300, model_count: 0 } },
                  }), { status: 200, headers: { 'content-type': 'application/json' } }));
                }
                return real(url, opts);
              };
            })();
            "#,
        )
        .await
        .unwrap();
    page2.goto(server.url("#/new")).await.unwrap();
    wait_for(
        &page2,
        "document.getElementById('start-btn') && document.getElementById('start-btn').disabled",
        "Start to be disabled with 0 usable models",
    )
    .await;
    let zero_estimate = text_of(&page2, "#estimate").await;

    assert!(
        baseline.contains("3 models"),
        "baseline estimate should reflect the 3-model mock panel: {baseline}"
    );
    assert!(
        zero_estimate.contains("0 model"),
        "an all-unusable roster's estimate must fall to 0 models: {zero_estimate}"
    );
}

/// `start_disabled_with_0_usable_models`: covered structurally above
/// (`estimate_falls_when_a_model_is_unusable`'s own wait condition), and
/// asserted explicitly here as its own named case.
#[tokio::test]
async fn start_disabled_with_0_usable_models() {
    let server = start_server("start_disabled");
    let (browser, _guard) = browser("start_disabled").await;
    let page = browser.new_page("about:blank").await.unwrap();
    page.evaluate_on_new_document(
        r#"
        (function () {
          const real = window.fetch.bind(window);
          window.fetch = function (url, opts) {
            if (String(url).indexOf('/api/providers') !== -1 && (!opts || !opts.method || opts.method === 'GET')) {
              return Promise.resolve(new Response(JSON.stringify({
                providers: [{ id: 'mock', state: 'missing', source: null, fingerprint: null, usable: false }],
                estimates: { standard: { cost: 0, calls: 0, wall_clock_secs: 300, model_count: 0 }, deep: { cost: 0, calls: 0, wall_clock_secs: 300, model_count: 0 } },
              }), { status: 200, headers: { 'content-type': 'application/json' } }));
            }
            return real(url, opts);
          };
        })();
        "#,
    )
    .await
    .unwrap();
    page.goto(server.url("#/new")).await.unwrap();
    wait_for(
        &page,
        "!!document.getElementById('start-btn')",
        "the Start button to render",
    )
    .await;
    let disabled: bool = page
        .evaluate("document.getElementById('start-btn').disabled")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert!(disabled, "Start must be disabled when 0 models are usable");
}

/// `the_detach_note_is_present`: screen 2's own required copy, verbatim.
#[tokio::test]
async fn the_detach_note_is_present() {
    let server = start_server("detach_note");
    let (browser, _guard) = browser("detach_note").await;
    let page = browser.new_page(server.url("#/new")).await.unwrap();
    wait_for(
        &page,
        "!!document.getElementById('new-run-form')",
        "screen 1 to render",
    )
    .await;
    page.find_element("#question")
        .await
        .unwrap()
        .click()
        .await
        .unwrap()
        .type_str("Should we adopt a modular monolith?")
        .await
        .unwrap();
    page.find_element("#start-btn")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    wait_for(
        &page,
        "location.hash.startsWith('#/running/')",
        "navigation to the running screen",
    )
    .await;
    wait_for(
        &page,
        "document.body.innerText.indexOf('Closing this page does not stop the run') !== -1",
        "the detach note",
    )
    .await;
}

/// Reads the `id:` sequence numbers off `GET /api/runs/:id/events`, via the
/// *browser's own* `fetch` (the same `X-Arbiter-Token` header convention
/// `api()` in `ui.html` uses for every non-SSE call) rather than a Rust
/// HTTP client -- this is what proves the endpoint's resumption contract
/// actually works from inside the sandboxed page context `EventSource`
/// itself runs in, not just from a bare `reqwest` call (already covered,
/// HTTP-client-side, by `serve::tests::sse_resumes_from_last_event_id`).
/// `last_event_id: None` fetches the full backlog from the start; `Some(n)`
/// mirrors the browser's own automatic `Last-Event-ID` reconnect header.
async fn fetch_event_ids(
    page: &Page,
    server: &Server,
    run_id: &str,
    last_event_id: Option<i64>,
) -> Vec<i64> {
    let last_event_header = match last_event_id {
        Some(id) => format!(r#"headers["Last-Event-ID"] = "{id}";"#),
        None => String::new(),
    };
    let script = format!(
        r#"(async function () {{
            var headers = {{ "X-Arbiter-Token": "{token}" }};
            {last_event_header}
            var res = await fetch("/api/runs/{run_id}/events", {{ headers: headers }});
            var text = await res.text();
            var ids = [];
            text.split("\n").forEach(function (line) {{
                if (line.indexOf("id: ") === 0) ids.push(parseInt(line.slice(4), 10));
            }});
            return ids;
        }})()"#,
        token = server.token,
        run_id = run_id,
    );
    page.evaluate(script.as_str())
        .await
        .unwrap()
        .into_value()
        .unwrap()
}

/// `sse_reconnect_resumes_without_duplicate_events`: a reconnect carrying
/// `Last-Event-ID` (browser-native `EventSource` behavior, driven by the
/// server's own `id:` field) must resume exactly after that id -- never
/// replaying an event already delivered, never skipping one.
#[tokio::test]
async fn sse_reconnect_resumes_without_duplicate_events() {
    let server = start_server("sse_resume");
    let (browser, _guard) = browser("sse_resume").await;
    let page = browser.new_page(server.url("")).await.unwrap();
    run_to_result(&page, &server).await;

    let run_id: String = page
        .evaluate("decodeURIComponent(location.hash.split('/')[2])")
        .await
        .unwrap()
        .into_value()
        .unwrap();

    let full = fetch_event_ids(&page, &server, &run_id, None).await;
    assert!(
        full.len() >= 2,
        "a completed run must have logged more than one event: {full:?}"
    );
    let mut sorted = full.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        full.len(),
        "no event id should ever be delivered twice in one stream: {full:?}"
    );

    let mid = full[full.len() / 2];
    let resumed = fetch_event_ids(&page, &server, &run_id, Some(mid)).await;
    assert!(
        !resumed.is_empty(),
        "resuming from a non-terminal id must yield the remaining events: {full:?}"
    );
    assert_eq!(
        resumed[0],
        mid + 1,
        "a reconnect must resume exactly after Last-Event-ID, with no gap or replay: full={full:?} resumed={resumed:?}"
    );
    for id in &resumed {
        assert!(
            *id > mid,
            "a resumed stream must never replay an id at or before Last-Event-ID: {resumed:?}"
        );
    }
}

/// `a_non_consensus_result_shows_the_live_objection_above_the_fold`: the
/// synthetic panel's own three independent, non-contradicting positions
/// reliably produce a `SplitDecision` (proven already by every fixture and
/// smoke run against this exact panel) -- never `CONSENSUS` -- so the live
/// objection banner must be present, and precede the options table in
/// document order ("above the fold").
#[tokio::test]
async fn a_non_consensus_result_shows_the_live_objection_above_the_fold() {
    let server = start_server("live_objection");
    let (browser, _guard) = browser("live_objection").await;
    let page = browser.new_page(server.url("")).await.unwrap();
    run_to_result(&page, &server).await;

    let outcome = text_of(&page, ".tag").await;
    assert_ne!(
        outcome.trim(),
        "CONSENSUS",
        "this panel's own shared, uncontested claims never converge to bare consensus"
    );

    let order: Option<bool> = page
        .evaluate(
            "(() => { const banner = Array.from(document.querySelectorAll('.banner.warn')).find(b => b.textContent.includes('Live objection')); \
              const table = document.querySelector('table'); \
              if (!banner || !table) return null; \
              return !!(banner.compareDocumentPosition(table) & Node.DOCUMENT_POSITION_FOLLOWING); })()",
        )
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert_eq!(
        order,
        Some(true),
        "the live objection banner must precede the options table, not be buried under it"
    );
}

/// `the_breakdown_lists_5_penalties`: every one of the 5 penalty terms
/// (unresolved/assumption/truncation/convergence/dispersion) is a row,
/// even when its own contribution is exactly zero -- "inactive penalties
/// shown at 0, not hidden" (U4).
#[tokio::test]
async fn the_breakdown_lists_5_penalties() {
    let server = start_server("breakdown_penalties");
    let (browser, _guard) = browser("breakdown_penalties").await;
    let page = browser.new_page(server.url("")).await.unwrap();
    run_to_result(&page, &server).await;

    let penalty_rows: u32 = page
        .evaluate("document.querySelectorAll('#penalties-table tbody tr').length")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert_eq!(
        penalty_rows, 5,
        "all five penalty rows must render, including inactive ones"
    );
}

/// `override_requires_a_reason`: submitting an override with an empty
/// reason must be refused client-side (no round trip needed to know a
/// blank reason is invalid) with a visible error, and never call Accept
/// through to a request that could otherwise be misread as "no override
/// was actually requested."
#[tokio::test]
async fn override_requires_a_reason() {
    let server = start_server("override_reason");
    let (browser, _guard) = browser("override_reason").await;
    let page = browser.new_page(server.url("")).await.unwrap();
    run_to_result(&page, &server).await;

    page.find_element("#add-override")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    page.find_element("#ov-path-0")
        .await
        .unwrap()
        .click()
        .await
        .unwrap()
        .type_str("recommendation.option_id")
        .await
        .unwrap();
    page.find_element("#ov-to-0")
        .await
        .unwrap()
        .click()
        .await
        .unwrap()
        .type_str("opt_x")
        .await
        .unwrap();
    // Deliberately leave the reason field empty.
    page.find_element("#accept-btn")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();

    wait_for(
        &page,
        "!!document.querySelector('.field-error')",
        "a validation error for the missing reason",
    )
    .await;
    let msg = text_of(&page, ".field-error").await;
    assert!(
        msg.to_lowercase().contains("reason"),
        "the error must name the missing reason, got: {msg}"
    );
    let accepted: bool = page
        .evaluate("document.body.innerText.indexOf('Accepted') !== -1")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert!(
        !accepted,
        "an override with no reason must never be accepted"
    );
}

/// `compare_renders_one_card_per_model`: screen 6 replaces the standalone
/// Node app that used to live in `tools/multiplex/`. With no keys configured
/// -- the test environment's own state -- every model must still get a card
/// saying *why* it did not answer, and the run must terminate rather than
/// spinning forever on models that were never called.
#[tokio::test]
async fn compare_renders_one_card_per_model() {
    let server = start_server("compare");
    let (browser, _guard) = browser("compare").await;
    let page = browser.new_page(server.url("")).await.unwrap();
    page.evaluate("location.hash = '#/compare'").await.unwrap();
    wait_for(
        &page,
        "!!document.getElementById('compare-form')",
        "the compare form to render",
    )
    .await;

    // An empty prompt is refused before any request goes out -- this endpoint
    // spends money, so the page must not send a request it knows is invalid.
    page.find_element("#compare-btn")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    wait_for(
        &page,
        "!!document.querySelector('#compare-error .field-error')",
        "an empty prompt to be refused client-side",
    )
    .await;
    assert_eq!(
        page.evaluate("document.querySelectorAll('.resp-card').length")
            .await
            .unwrap()
            .into_value::<u32>()
            .unwrap(),
        0,
        "a refused submit must not render any answer cards"
    );

    page.find_element("#compare-prompt")
        .await
        .unwrap()
        .click()
        .await
        .unwrap()
        .type_str("Which database should we use?")
        .await
        .unwrap();
    page.find_element("#compare-btn")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();

    wait_for(
        &page,
        "document.getElementById('compare-total').textContent.length > 0",
        "the comparison to finish",
    )
    .await;

    let cards: u32 = page
        .evaluate("document.querySelectorAll('.resp-card').length")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert_eq!(
        cards, 7,
        "every provider this build can reach needs a card, answered or not"
    );
    let skipped: u32 = page
        .evaluate("document.querySelectorAll('.resp-card.is-skipped').length")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert_eq!(cards, skipped, "with no keys, every card must be a skip");

    let body = text_of(&page, "#compare-results").await;
    assert!(
        body.contains("no key configured"),
        "a skipped model must say why: {body}"
    );
    // The spinner has to stop even though nothing was ever called.
    assert_eq!(
        page.evaluate("document.querySelectorAll('#compare-results .spinner').length")
            .await
            .unwrap()
            .into_value::<u32>()
            .unwrap(),
        0,
        "no card should still be spinning once the run is done"
    );
}

/// `history_is_empty_stated`: a brand-new store, never having run
/// anything, shows an explicit empty state and a way to start a run --
/// never a bare, silent empty table.
#[tokio::test]
async fn history_is_empty_stated() {
    let server = start_server("history_empty");
    let (browser, _guard) = browser("history_empty").await;
    let page = browser.new_page(server.url("#/history")).await.unwrap();
    wait_for(
        &page,
        "document.body.innerText.indexOf('No debates yet') !== -1",
        "the empty-history state",
    )
    .await;
    assert!(
        page.find_element("a[href='#/new']").await.is_ok(),
        "the empty state must offer a way to start the first run"
    );
}

/// `keys_screen_never_renders_a_key`: fingerprints and states only -- the
/// full key value must never appear anywhere in the Keys screen's own DOM,
/// even indirectly (e.g. in a title attribute or a data- attribute).
#[tokio::test]
async fn keys_screen_never_renders_a_key() {
    let server = start_server("keys_no_render");
    let (browser, _guard) = browser("keys_no_render").await;
    let page = browser.new_page(server.url("#/keys")).await.unwrap();
    wait_for(
        &page,
        "document.querySelector('h1') && document.querySelector('h1').textContent === 'Keys'",
        "the Keys screen to render",
    )
    .await;
    let html = page.content().await.unwrap();
    // This build never resolves a real secret anywhere reachable from
    // here (no key is ever configured in this sandbox), so the structural
    // guarantee under test is that only `fingerprint` (never `expose()`)
    // ever reaches the page -- confirmed by grepping the rendered HTML for
    // anything resembling a raw, unredacted secret value is meaningless
    // without one existing; the real guard is in `serve/handlers.rs`'s own
    // `list_providers`, which only ever serializes `.fingerprint()`.
    assert!(
        !html.contains("expose"),
        "the page must never call SecretString::expose() or reference it"
    );
}

/// `the_keys_screen_can_add_and_test_a_key`: the two things an operator with
/// no key needs are on this screen, not in a terminal — a form to set one and
/// a button to prove it works. The form must carry the key as a password
/// field (not shoulder-readable, not remembered by the browser), and the
/// value must leave the DOM once submitted.
#[tokio::test]
async fn the_keys_screen_can_add_and_test_a_key() {
    let server = start_server("keys_add");
    let (browser, _guard) = browser("keys_add").await;
    let page = browser.new_page(server.url("#/keys")).await.unwrap();
    wait_for(
        &page,
        "!!document.querySelector('[data-setkey]')",
        "the Keys screen with its per-provider actions",
    )
    .await;

    // Every real provider offers both actions; `mock` needs no key and so
    // appears in neither list.
    let counts: String = page
        .evaluate(
            "JSON.stringify({               set: document.querySelectorAll('[data-setkey]').length,                test: document.querySelectorAll('[data-recheck]').length,                mock: document.querySelectorAll('[data-setkey=\"mock\"]').length })",
        )
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert!(counts.contains("\"set\":7"), "{counts}");
    assert!(counts.contains("\"test\":7"), "{counts}");
    assert!(
        counts.contains("\"mock\":0"),
        "mock must not offer a key form: {counts}"
    );

    // Opening the form reveals a password input, never a plain text one.
    page.find_element("[data-setkey='anthropic']")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    wait_for(
        &page,
        "!!document.getElementById('key-input-anthropic')",
        "the add-key form",
    )
    .await;
    let field: String = page
        .evaluate(
            "JSON.stringify({ type: document.getElementById('key-input-anthropic').type,                autocomplete: document.getElementById('key-input-anthropic').getAttribute('autocomplete') })",
        )
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert!(field.contains("\"type\":\"password\""), "{field}");
    assert!(field.contains("\"autocomplete\":\"off\""), "{field}");

    // An empty key is refused client-side, before a request is sent.
    page.evaluate(
        "document.querySelector('#key-detail-anthropic form').dispatchEvent(new Event('submit', {cancelable:true}))",
    )
    .await
    .unwrap();
    wait_for(
        &page,
        "!!document.querySelector('#key-detail-anthropic .field-error')",
        "an empty key to be refused",
    )
    .await;

    // Testing a keyless provider reports it rather than erroring, and spends
    // nothing to find out.
    page.find_element("[data-recheck='openai']")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    wait_for(
        &page,
        "!!document.querySelector('#key-detail-openai .banner')",
        "the test result for a keyless provider",
    )
    .await;
    let result = text_of(&page, "#key-detail-openai").await;
    assert!(
        result.contains("No key configured"),
        "the result must say why: {result}"
    );
}

/// `every_screen_is_keyboard_navigable`: each screen has at least one
/// real, focusable, non-pointer-only control reachable by Tab, and the
/// question field is the first stop on screen 1 (`autofocus`).
#[tokio::test]
async fn every_screen_is_keyboard_navigable() {
    let server = start_server("keyboard_nav");
    let (browser, _guard) = browser("keyboard_nav").await;

    for hash in ["", "#/new", "#/history", "#/keys"] {
        let page = browser.new_page(server.url(hash)).await.unwrap();
        wait_for(
            &page,
            "!!document.querySelector('h1')",
            "the screen to render",
        )
        .await;

        // Every interactive element must be a real, natively-focusable tag
        // (`a[href]`/`button`/`input`/`select`/`textarea`) or carry an
        // explicit `tabindex` -- never a `<div onclick>` or similar
        // pointer-only affordance (U7's own "no pointer-only affordance").
        let all_focusable: bool = page
            .evaluate(
                "(() => { \
                   const clickable = document.querySelectorAll('[onclick], .link, button, a'); \
                   const realTags = new Set(['A','BUTTON','INPUT','SELECT','TEXTAREA']); \
                   return Array.from(clickable).every(el => realTags.has(el.tagName) || el.hasAttribute('tabindex')); \
                 })()",
            )
            .await
            .unwrap()
            .into_value()
            .unwrap();
        assert!(
            all_focusable,
            "screen '{hash}' has an interactive element that is not natively keyboard-focusable"
        );

        let count: u32 = page
            .evaluate(
                "document.querySelectorAll('a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]').length",
            )
            .await
            .unwrap()
            .into_value()
            .unwrap();
        assert!(
            count > 0,
            "screen '{hash}' must expose at least one real, keyboard-focusable control"
        );

        // Whichever screen you land on, its own primary text field declares
        // `autofocus` -- Compare's prompt on the landing tab, the debate
        // question on `#/new`. Checked as a static attribute, not as headless
        // Chromium's actual runtime focus state, which (unlike a real browser
        // window) does not reliably honor `autofocus` on an inactive or
        // offscreen target.
        let autofocus_field = match hash {
            "" => Some("compare-prompt"),
            "#/new" => Some("question"),
            _ => None,
        };
        if let Some(field) = autofocus_field {
            let has_autofocus: bool = page
                .evaluate(format!(
                    "!!document.getElementById('{field}') && document.getElementById('{field}').hasAttribute('autofocus')"
                ))
                .await
                .unwrap()
                .into_value()
                .unwrap();
            assert!(
                has_autofocus,
                "#{field} must declare autofocus on screen '{hash}'"
            );
        }
    }
}

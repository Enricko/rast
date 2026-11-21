//! Browser sign-in bridge. The HTML form signs in to the RAST server, which
//! sets an HttpOnly session cookie; JavaScript never sees the session itself.
//! Rune starts only after the server has answered who is signed in, and
//! unmounts on sign-out.

use std::cell::RefCell;

use rune::{App, AppHandle, Tx};
use wasm_bindgen::prelude::*;

use crate::{State, app_spec, config::Config, server};

thread_local! {
    static WORKSPACE: RefCell<Option<AppHandle<State>>> = const { RefCell::new(None) };
}

/// Open the desktop for the account the server just signed in (`user` is the
/// server's JSON for it). Roles are only shown here; the server enforces them.
#[wasm_bindgen]
pub fn open_workspace(user: String) -> Result<(), JsValue> {
    let account: server::Account = serde_json::from_str(&user).map_err(|_| {
        JsValue::from_str(
            "The RAST server sent an account this desktop doesn't understand. Reload the page.",
        )
    })?;
    if WORKSPACE.with_borrow(Option::is_some) {
        return Err(JsValue::from_str("The workspace is already open."));
    }
    let app = App::new(app_spec())
        .font_smoothing(crate::font_smoothing())
        .init(move || State::with_session(Config::load(), account));
    let handle = app.handle();
    app.try_run()
        .map_err(|message| JsValue::from_str(&message))?;
    WORKSPACE.with_borrow_mut(|slot| *slot = Some(handle));
    Ok(())
}

#[wasm_bindgen]
pub fn workspace_error() -> Option<String> {
    WORKSPACE.with_borrow(|slot| slot.as_ref().and_then(AppHandle::window_error))
}

#[wasm_bindgen]
pub fn close_workspace() {
    if let Some(handle) = WORKSPACE.with_borrow_mut(Option::take) {
        handle.quit();
    }
}

/// Sign out: close the desktop and let the page end the server session.
pub(crate) fn sign_out(state: &mut State, tx: &mut Tx) {
    sign_out_with(state, tx, "");
}

/// Sign out and show `reason` on the sign-in form (empty for none).
pub(crate) fn sign_out_with(state: &mut State, tx: &mut Tx, reason: &str) {
    state.sign_out();
    close_workspace();
    tx.quit();
    if let Some(window) = web_sys::window() {
        let _ = js_sys::Reflect::set(
            &window,
            &JsValue::from_str("rastSignOutReason"),
            &JsValue::from_str(reason),
        );
        if let Ok(event) = web_sys::Event::new("rastsignout") {
            let _ = window.dispatch_event(&event);
        }
    }
}

//! Browser login bridge. The HTML form reflows without Rune; start the canvas
//! only after the local password is verified, and unmount it on sign-out.

use std::cell::RefCell;

use rune::{App, AppHandle, TextEdit, Tx};
use wasm_bindgen::prelude::*;

use crate::{Screen, State, app_spec, auth::DeviceAccount, config::Config};

thread_local! {
    static WORKSPACE: RefCell<Option<AppHandle<State>>> = const { RefCell::new(None) };
}

#[wasm_bindgen]
pub fn device_username() -> Result<Option<String>, JsValue> {
    Ok(load_account()?.map(|account| account.username))
}

/// Check credentials without exposing the verifier or using a network request.
/// Reload storage at submit time so setup does not replace an existing account.
#[wasm_bindgen]
pub fn unlock_workspace(username: String, password: String, creating: bool) -> Result<(), JsValue> {
    let password = TextEdit::new(password).secure();
    let account = authenticate(&username, &password.text, creating)?;
    drop(password);
    if WORKSPACE.with_borrow(Option::is_some) {
        return Err(JsValue::from_str("The workspace is already open."));
    }
    let app = App::new(app_spec()).init(move || {
        let mut state = State::with_account(Config::load(), Some(account));
        state.signed_in = true;
        state.screen = Screen::Desktop;
        state
    });
    let handle = app.handle();
    app.try_run()
        .map_err(|message| JsValue::from_str(&message))?;
    WORKSPACE.with_borrow_mut(|slot| *slot = Some(handle));
    Ok(())
}

fn load_account() -> Result<Option<DeviceAccount>, JsValue> {
    DeviceAccount::load_browser().map_err(|error| JsValue::from_str(&error.to_string()))
}

fn authenticate(username: &str, password: &str, creating: bool) -> Result<DeviceAccount, JsValue> {
    let incorrect = || JsValue::from_str("The username or password is incorrect.");
    match (load_account()?, creating) {
        (None, true) => {
            let account = DeviceAccount::create(username, password)
                .map_err(|error| JsValue::from_str(&error))?;
            account
                .save_browser()
                .map_err(|error| JsValue::from_str(&error.to_string()))?;
            Ok(account)
        }
        (Some(account), false) => {
            if username.trim() == account.username
                && password.len() <= crate::auth::MAX_PASSWORD_LEN
                && account
                    .verify(password)
                    .map_err(|error| JsValue::from_str(&error))?
            {
                Ok(account)
            } else {
                Err(incorrect())
            }
        }
        _ => Err(JsValue::from_str(
            "The local account changed. Reload RAST and try again.",
        )),
    }
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

pub(crate) fn sign_out(state: &mut State, tx: &mut Tx) {
    state.sign_out();
    close_workspace();
    tx.quit();
    if let Some(window) = web_sys::window()
        && let Ok(event) = web_sys::Event::new("rastsignout")
    {
        let _ = window.dispatch_event(&event);
    }
}

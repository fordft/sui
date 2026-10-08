use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sui::auth::{login::Prompt, LoginProvider};
use sui::config::{ProfileCfg, Transport, UiSettings};
use sui::tui::app::{App, Effect, Field, Modal, ProvForm, ProvType};
fn app() -> App {
    App::with_state(
        std::env::temp_dir(),
        std::collections::BTreeMap::from([(
            "existing".into(),
            ProfileCfg {
                base_url: Some("http://127.0.0.1:9/v1".into()),
                model: Some("old-model".into()),
                ..Default::default()
            },
        )]),
        UiSettings::default(),
    )
}
fn press(app: &mut App, key: KeyCode) {
    app.key(KeyEvent::new(key, KeyModifiers::NONE));
}
fn field(app: &mut App, field: Field) {
    let Some(Modal::Provider(form)) = &mut app.modal else {
        panic!("provider form required")
    };
    form.focus = form.fields().iter().position(|f| *f == field).unwrap();
}
fn start_login(app: &mut App, ptype: ProvType) -> u64 {
    app.modal = Some(Modal::Provider(ProvForm::new(ptype)));
    field(app, Field::SignIn);
    press(app, KeyCode::Enter);
    let Some(Modal::Login(dialog)) = &app.modal else {
        panic!("login dialog missing")
    };
    dialog.request
}
#[test]
fn all_account_providers_have_native_sign_in_and_catalog_effects() {
    for (ptype, provider) in [
        (ProvType::Codex, LoginProvider::Codex),
        (ProvType::Copilot, LoginProvider::Copilot),
    ] {
        let mut app = app();
        let request = start_login(&mut app, ptype);
        assert!(app.effects.iter().any(
            |e| matches!(e,Effect::SignIn {provider:p,request:r} if *p == provider && *r == request)
        ));
        app.login_finished(request, Ok(()));
        field(&mut app, Field::Model);
        press(&mut app, KeyCode::Enter);
        assert!(app.effects.iter().any(
            |e| matches!(e,Effect::FetchModels {transport,..} if *transport == provider.transport())
        ));
        press(&mut app, KeyCode::Esc);
        field(&mut app, Field::Save);
        press(&mut app, KeyCode::Enter);
        assert!(app
            .effects
            .iter()
            .any(|e| matches!(e,Effect::SaveProfile {kind,key,key_env,..}
            if kind.as_deref() == Some(provider.kind()) && key.is_none() && key_env.is_none())));
    }
}
#[test]
fn login_input_is_masked_kept_out_of_chat_and_late_replies_are_ignored() {
    let mut app = app();
    app.input.set("keep my task draft");
    let request = start_login(&mut app, ProvType::Codex);
    let before = app.signed_in.clone();
    app.login_prompt(
        request,
        Prompt {
            url: "https://auth.openai.com/oauth/authorize".into(),
            user_code: None,
            input_required: true,
        },
    );
    app.paste("private-authorization-code");
    assert_eq!(app.input.text(), "keep my task draft");
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 32)).unwrap();
    sui::tui::slime::freeze_clock(Some(0));
    terminal.draw(|f| sui::tui::draw::draw(f, &app)).unwrap();
    sui::tui::slime::freeze_clock(None);
    let displayed = format!("{}", terminal.backend());
    assert!(displayed.contains("Authorization input"));
    assert!(!displayed.contains("private-authorization-code"));
    app.login_finished(request + 1, Ok(()));
    assert!(matches!(&app.modal, Some(Modal::Login(_))));
    press(&mut app, KeyCode::Enter);
    assert!(app.effects.iter().any(|e| matches!(e,Effect::SignInInput {request:r,input} if *r == request && input == "private-authorization-code")));
    let Some(Modal::Login(dialog)) = &app.modal else {
        panic!()
    };
    assert!(dialog.input.text().is_empty() && dialog.submitted);
    press(&mut app, KeyCode::Esc);
    assert!(app
        .effects
        .iter()
        .any(|e| matches!(e,Effect::CancelSignIn {request:r} if *r == request)));
    app.login_finished(request, Ok(()));
    assert!(matches!(app.modal, Some(Modal::Provider(_))));
    assert_eq!(app.signed_in, before);
}
#[test]
fn local_models_need_no_key() {
    let mut app = app();
    let mut form = ProvForm::new(ProvType::Ollama);
    form.model.set("local-coder");
    app.modal = Some(Modal::Provider(form));
    field(&mut app, Field::Model);
    press(&mut app, KeyCode::Enter);
    assert!(app.effects.iter().any(
        |e| matches!(e,Effect::FetchModels {transport:Transport::Ollama,key:None,base_url,..}
        if base_url == "http://127.0.0.1:11434/v1")
    ));
    press(&mut app, KeyCode::Esc);
}
#[test]
fn signing_in_cannot_replace_credentials_during_an_active_run() {
    let mut app = app();
    app.running = true;
    app.modal = Some(Modal::Provider(ProvForm::new(ProvType::Codex)));
    field(&mut app, Field::SignIn);
    press(&mut app, KeyCode::Enter);
    assert!(matches!(app.modal, Some(Modal::Provider(_))));
    assert!(!app
        .effects
        .iter()
        .any(|e| matches!(e, Effect::SignIn { .. })));
}

#[test]
fn custom_saved_key_works_when_editing_a_profile() {
    let mut app = app();
    let profile = ProfileCfg {
        base_url: Some("http://127.0.0.1:9/v1".into()),
        model: Some("custom-test".into()),
        api_key: Some("private-saved-key".into()),
        ..Default::default()
    };
    app.profiles.insert("custom".into(), profile.clone());
    app.modal = Some(Modal::Provider(ProvForm::from_existing(
        "custom", &profile, false,
    )));
    field(&mut app, Field::Model);
    press(&mut app, KeyCode::Enter);
    assert!(app.effects.iter().any(|e| matches!(e,
        Effect::FetchModels {transport:Transport::ChatCompletions,key,..}
        if key.as_deref() == Some("private-saved-key")
    )));
    press(&mut app, KeyCode::Esc);
    field(&mut app, Field::Test);
    press(&mut app, KeyCode::Enter);
    assert!(app.effects.iter().any(|e| matches!(e,
        Effect::Probe {transport:Transport::ChatCompletions,key,model,..}
        if key.as_deref() == Some("private-saved-key") && model == "custom-test"
    )));
}

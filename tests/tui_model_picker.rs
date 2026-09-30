use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sui::config::{ProfileCfg, UiSettings};
use sui::provider::ModelInfo;
use sui::tui::app::{Act, App, Effect, Field, Modal, ProvForm, ProvType, Role, SettingsRow};

fn enter(app: &mut App) {
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
}

fn app() -> App {
    App::with_state(
        std::env::temp_dir(),
        BTreeMap::from([(
            "gateway".into(),
            ProfileCfg {
                base_url: Some("http://127.0.0.1:9/v1".into()),
                model: Some("old-model".into()),
                ..Default::default()
            },
        )]),
        UiSettings::default(),
    )
}

fn catalog() -> Vec<ModelInfo> {
    vec![ModelInfo {
        id: "cx/gpt-6-sol".into(),
        context_length: Some(272_000),
        price_in: Some(0.000002),
        price_out: Some(0.000008),
        tools_claimed: Some(true),
    }]
}

fn request(app: &App) -> u64 {
    app.effects
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::FetchModels { request, .. } => Some(*request),
            _ => None,
        })
        .unwrap()
}

fn open_role_model(app: &mut App) {
    let row = app
        .settings_rows()
        .iter()
        .position(|r| matches!(r, SettingsRow::Role(Role::Solo)))
        .unwrap();
    app.settings_activate(row);
    enter(app);
}

#[test]
fn catalog_selection_saves_only_model_id_for_role() {
    let mut app = app();
    open_role_model(&mut app);
    app.models_loaded(request(&app), catalog(), None);
    enter(&mut app);
    assert_eq!(
        app.profiles["gateway"].model.as_deref(),
        Some("cx/gpt-6-sol")
    );
    assert!(app.effects.iter().any(|e| matches!(e,
        Effect::SaveProfile { model, .. } if model == "cx/gpt-6-sol"
    )));
}

#[test]
fn catalog_selection_saves_only_model_id_in_provider_form() {
    let mut app = app();
    let mut form = ProvForm::new(ProvType::Custom);
    form.name.set("new-gateway");
    form.focus = form
        .fields()
        .iter()
        .position(|f| *f == Field::Model)
        .unwrap();
    app.modal = Some(Modal::Provider(form));
    enter(&mut app);
    app.models_loaded(request(&app), catalog(), None);
    enter(&mut app);
    let Some(Modal::Provider(form)) = &mut app.modal else {
        panic!("model selection did not return to form");
    };
    assert_eq!(form.model.text(), "cx/gpt-6-sol");
    form.focus = form
        .fields()
        .iter()
        .position(|f| *f == Field::Save)
        .unwrap();
    enter(&mut app);
    assert!(app.effects.iter().any(|e| matches!(e,
        Effect::SaveProfile { model, .. } if model == "cx/gpt-6-sol"
    )));
}

#[test]
fn repeated_provider_error_is_shown_once_and_preserves_outcome() {
    use sui::events::UiEvent;
    let mut app = app();
    for _ in 0..2 {
        app.apply_event(UiEvent::Error {
            run: 1,
            agent: "solo".into(),
            msg: "provider http 400: unsupported model".into(),
        });
    }
    let outcome = "error: provider http 400: unsupported model";
    app.apply_event(UiEvent::RunDone {
        run: 1,
        outcome: outcome.into(),
        accepted_sha: None,
    });
    let group = app.groups.iter().find(|g| g.id == 1).unwrap();
    assert!(group.done && group.failed);
    assert_eq!(app.outcome, outcome);
    assert_eq!(group.outcome, outcome);
    let notes: Vec<_> = group
        .items
        .iter()
        .filter_map(|item| match item {
            Act::Note { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(notes, [outcome, "run finished: error"]);
}

#[test]
fn metadata_filter_selects_the_matching_id_and_keeps_display_details() {
    let mut app = app();
    open_role_model(&mut app);
    let mut models = catalog();
    models.insert(
        0,
        ModelInfo {
            id: "another-model".into(),
            context_length: Some(4096),
            price_in: None,
            price_out: None,
            tools_claimed: Some(false),
        },
    );
    app.models_loaded(request(&app), models, None);
    let Some(Modal::Picker(p)) = &mut app.modal else {
        panic!("missing picker")
    };
    assert_eq!(
        p.items[1].label,
        "cx/gpt-6-sol ctx=272000 tools $2.00/$8.00per-M"
    );
    p.filter.set("ctx=272000");

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    sui::tui::slime::freeze_clock(Some(0));
    terminal
        .draw(|frame| sui::tui::draw::draw(frame, &app))
        .unwrap();
    sui::tui::slime::freeze_clock(None);
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("ctx=272000 tools $2.00/$8.00per-M"));
    enter(&mut app);
    assert_eq!(
        app.profiles["gateway"].model.as_deref(),
        Some("cx/gpt-6-sol")
    );
}

#[test]
fn delayed_catalog_cannot_replace_another_dialog_or_newer_fetch() {
    let mut app = app();
    open_role_model(&mut app);
    let old_request = request(&app);
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let add_row = app
        .settings_rows()
        .iter()
        .position(|r| matches!(r, SettingsRow::AddProfile))
        .unwrap();
    app.settings_activate(add_row);
    app.models_loaded(old_request, catalog(), None);
    let Some(Modal::Picker(p)) = &app.modal else {
        panic!("missing provider-type picker")
    };
    assert!(p
        .items
        .iter()
        .any(|item| item.value == ProvType::Custom.name()));
    assert!(!p.items.iter().any(|item| item.value == "cx/gpt-6-sol"));

    open_role_model(&mut app);
    let new_request = request(&app);
    assert_ne!(new_request, old_request);
    app.models_loaded(old_request, vec![], Some("old catalog failed".into()));
    let Some(Modal::Picker(p)) = &app.modal else {
        panic!("missing newer model picker")
    };
    assert!(p.loading);
    assert!(p.items.is_empty());
    app.models_loaded(new_request, catalog(), None);
    // Duplicate/stale replies must also leave an already loaded catalog alone.
    app.models_loaded(new_request, vec![], Some("duplicate reply".into()));
    enter(&mut app);
    assert_eq!(
        app.profiles["gateway"].model.as_deref(),
        Some("cx/gpt-6-sol")
    );
}

#[test]
fn model_ids_and_manual_entry_are_preserved_verbatim() {
    let mut app = app();
    open_role_model(&mut app);
    let mut models = catalog();
    models[0].id = "literal ctx=123 tools".into();
    app.models_loaded(request(&app), models, None);
    enter(&mut app);
    assert_eq!(
        app.profiles["gateway"].model.as_deref(),
        Some("literal ctx=123 tools")
    );

    open_role_model(&mut app);
    let pending_request = request(&app);
    let Some(Modal::Picker(p)) = &mut app.modal else {
        panic!("missing picker")
    };
    p.filter.set("manual model / exact");
    enter(&mut app); // manual IDs also work before the catalog responds
    app.models_loaded(pending_request, catalog(), None);
    assert!(app.modal.is_none());
    assert_eq!(
        app.profiles["gateway"].model.as_deref(),
        Some("manual model / exact")
    );

    open_role_model(&mut app);
    app.models_loaded(request(&app), vec![], Some("unavailable".into()));
    let Some(Modal::Picker(p)) = &mut app.modal else {
        panic!("missing picker")
    };
    p.filter.set("manual-after-error");
    enter(&mut app);
    assert_eq!(
        app.profiles["gateway"].model.as_deref(),
        Some("manual-after-error")
    );
}

#[tokio::test]
async fn selected_model_survives_save_reload_and_reaches_the_request_verbatim() {
    let (tx, rx) = std::sync::mpsc::channel();
    let port = common::serve(move |body, _messages| {
        let body: serde_json::Value = serde_json::from_slice(body).unwrap();
        tx.send(body["model"].as_str().unwrap().to_string())
            .unwrap();
        common::sse_text("ok")
    });
    let mut app = app();
    let base = format!("http://127.0.0.1:{port}/v1");
    app.profiles.get_mut("gateway").unwrap().base_url = Some(base.clone());
    open_role_model(&mut app);
    app.models_loaded(request(&app), catalog(), None);
    enter(&mut app);
    let Effect::SaveProfile {
        name,
        base_url,
        model,
        key_env,
        key,
        kind,
        ..
    } = app
        .effects
        .iter()
        .find(|e| matches!(e, Effect::SaveProfile { .. }))
        .unwrap()
    else {
        unreachable!()
    };
    assert_eq!(base_url, &base);
    let cfg = std::env::temp_dir().join(format!(
        "sui-model-picker-{}-{}.toml",
        std::process::id(),
        port
    ));
    sui::config::save_profile_at(
        &cfg,
        name,
        base_url,
        model,
        key_env.as_deref(),
        key.as_deref(),
        kind.as_deref(),
    )
    .unwrap();
    let doc: toml::Value = std::fs::read_to_string(&cfg).unwrap().parse().unwrap();
    let reloaded: ProfileCfg = doc["profiles"][name].clone().try_into().unwrap();
    assert_eq!(reloaded.model.as_deref(), Some("cx/gpt-6-sol"));
    std::fs::remove_file(cfg).unwrap();
    let provider = sui::provider::Provider::new(
        reloaded.base_url.as_deref().unwrap(),
        None,
        reloaded.model.unwrap(),
        None,
    );
    let messages = [sui::types::Message::User {
        content: "hi".into(),
    }];
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        provider.stream_chat(
            &sui::context::Compiled::view(&messages),
            &[],
            |_| {},
            |_| {},
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(outcome.content, "ok");
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap(),
        "cx/gpt-6-sol"
    );
}

#[test]
fn different_errors_and_unreported_failures_remain_visible() {
    use sui::events::UiEvent;
    let mut app = app();
    for (agent, msg) in [("solo", "first"), ("worker", "first"), ("worker", "second")] {
        app.apply_event(UiEvent::Error {
            run: 1,
            agent: agent.into(),
            msg: msg.into(),
        });
    }
    let group = app.groups.iter().find(|g| g.id == 1).unwrap();
    assert_eq!(group.items.len(), 3);
    app.apply_event(UiEvent::RunDone {
        run: 2,
        outcome: "error: not previously reported".into(),
        accepted_sha: None,
    });
    let group = app.groups.iter().find(|g| g.id == 2).unwrap();
    assert!(matches!(&group.items[0], Act::Note { text, err: true, .. }
        if text == "run finished: error: not previously reported"));
}
mod common;

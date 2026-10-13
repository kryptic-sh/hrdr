//! The terminal title follows the session: status emoji, working directory,
//! session name.

use super::*;

fn place(h: &Harness) -> String {
    h.app
        .dir_cwd
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[tokio::test]
async fn the_title_tracks_working_done_and_idle() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![MockReply::Text("all set.".into())]).await;
    assert_eq!(h.app.terminal_title(), format!("💤 {}", place(&h)));

    h.app.registry.begin_turn(hrdr_agent::MAIN_KEY);
    assert!(h.app.terminal_title().starts_with("⏳ "));
    h.app.registry.end_turn(hrdr_agent::MAIN_KEY);

    h.submit("tidy the readme").await;
    assert_eq!(
        h.app.terminal_title(),
        format!("✅ {} · tidy the readme", place(&h)),
        "a finished turn shows as done, under the session's name"
    );

    h.app.on_mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Moved,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    });
    assert!(
        h.app.terminal_title().starts_with("✅ "),
        "the pointer passing over is not the user attending"
    );
    h.press(KeyCode::Char('x'));
    assert!(
        h.app.terminal_title().starts_with("💤 "),
        "once the user is back, the session is idle"
    );
}

#[tokio::test]
async fn a_turn_that_ends_on_a_question_waits_for_the_user() {
    let _data_home = isolated_data_home();
    let mut h = Harness::new(vec![MockReply::Text(
        "Should I also update the tests?".into(),
    )])
    .await;
    h.submit("fix the parser").await;
    assert!(
        h.app.terminal_title().starts_with("❓ "),
        "{}",
        h.app.terminal_title()
    );
    h.press(KeyCode::Char('y'));
    assert!(h.app.terminal_title().starts_with("💤 "));
}

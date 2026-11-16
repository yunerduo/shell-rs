//! AI 助手: pictures and files wait in the composer, can be dropped again,
//! and a model marked as unable to see keeps pictures out.

use shellrs::assistant::settings::ModelProfile;

use crate::support::*;

/// A one-pixel PNG: enough for the attachment to be read as a picture.
const SHOT: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0x60, 0xf8, 0x5f,
    0x0f, 0x00, 0x02, 0x87, 0x01, 0x80, 0xeb, 0x47, 0xba, 0x92, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

fn file(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).expect("the fixture file");
    path
}

/// The assistant, opened beside web-01's terminal the way a user opens it.
async fn open_assistant(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<Workspace>) {
    let (handle, workspace) = open_workspace(cx);
    in_frame(cx, handle, |window, cx| {
        window.activate_window();
        window.click(("terminal-tab", INITIAL_WEB_TERMINAL), cx)
    });
    in_frame(cx, handle, |window, cx| window.click("tool-assistant", cx));
    (handle, workspace)
}

#[gpui_kit::test]
async fn a_picture_and_a_file_wait_in_the_composer_until_they_are_dropped(cx: &mut TestAppContext) {
    let (handle, workspace) = open_assistant(cx).await;
    let dir = scratch();
    let paths = vec![
        file(&dir, "shot.png", SHOT),
        file(&dir, "run.log", "one\ntwo\n".as_bytes()),
    ];
    let panel = cx.update(|cx| workspace.read(cx).assistant(cx));
    cx.update(|cx| panel.update(cx, |panel, cx| panel.attach_paths(paths, cx)));
    cx.run_until_parked();

    in_frame(cx, handle, |window, _| {
        assert!(window.find(("assistant-attachment", 0usize)).visible());
        assert!(window.find(("assistant-attachment", 1usize)).visible());
        // Nothing typed, but the pictures alone are a message to send.
        assert_ne!(window.find("assistant-send").disabled(), Some(true));
    });

    // The × on the picture's corner takes it off; the file moves up.
    in_frame(cx, handle, |window, cx| {
        window.click(("assistant-attachment-remove", 0usize), cx)
    });
    cx.run_until_parked();

    in_frame(cx, handle, |window, _| {
        assert!(window.try_find(("assistant-attachment", 1usize)).is_none());
        assert!(window.find(("assistant-attachment", 0usize)).visible());
        assert_ne!(window.find("assistant-send").disabled(), Some(true));
    });
}

#[gpui_kit::test]
async fn a_text_only_model_keeps_a_picture_out_of_the_composer(cx: &mut TestAppContext) {
    let (handle, workspace) = open_assistant(cx).await;
    let settings = cx.update(|cx| workspace.read(cx).settings().clone());
    cx.update(|cx| {
        settings.update(cx, |settings, cx| {
            settings.update(
                |settings| {
                    let mut profile = ModelProfile {
                        model: "text-only".into(),
                        supports_images: false,
                        ..Default::default()
                    };
                    profile.id = ModelProfile::id_of(profile.protocol, &profile.model);
                    settings.assistant.active_model = profile.id.clone();
                    settings.assistant.models.push(profile);
                },
                cx,
            )
        })
    });
    cx.run_until_parked();

    let dir = scratch();
    let panel = cx.update(|cx| workspace.read(cx).assistant(cx));
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            panel.attach_paths(vec![file(&dir, "shot.png", SHOT)], cx)
        })
    });
    cx.run_until_parked();

    in_frame(cx, handle, |window, _| {
        assert!(window.try_find(("assistant-attachment", 0usize)).is_none());
        assert!(window.find("assistant-notice").visible());
    });

    // A file is not a picture: it still goes in.
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            panel.attach_paths(vec![file(&dir, "run.log", b"one\ntwo\n")], cx)
        })
    });
    cx.run_until_parked();
    in_frame(cx, handle, |window, _| {
        assert!(window.find(("assistant-attachment", 0usize)).visible());
    });

    // The same model with pictures on takes one.
    cx.update(|cx| {
        settings.update(cx, |settings, cx| {
            settings.update(
                |settings| {
                    if let Some(profile) = settings.assistant.models.first_mut() {
                        profile.supports_images = true;
                    }
                },
                cx,
            )
        })
    });
    cx.run_until_parked();
    cx.update(|cx| {
        panel.update(cx, |panel, cx| {
            panel.attach_paths(vec![file(&dir, "shot.png", SHOT)], cx)
        })
    });
    cx.run_until_parked();
    in_frame(cx, handle, |window, _| {
        assert!(window.find(("assistant-attachment", 1usize)).visible());
    });
}

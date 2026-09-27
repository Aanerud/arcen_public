use std::sync::Mutex;

/// Actions the native macOS menu bar dispatches back into the egui app.
///
/// The menu items live in AppKit and fire on the main thread; each one queues
/// a command here that `ArcenApp` drains once per frame in `logic()`. This
/// is what lets the single native menu drive the session (there is no longer a
/// duplicate in-app menu bar in the viewer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCommand {
    /// Quit, from the app menu or Cmd+Q. `from_keyboard` is decided when
    /// the menu fires: a Cmd+Q in a session is hold-to-quit, a click quits.
    Quit {
        from_keyboard: bool,
    },
    ToggleHealth,
    Disconnect,
    ReleaseModifiers,
    ToggleFullscreen,
    TogglePointerLock,
    ToggleTabletMonitor,
    /// Mirror the tablet X axis and negate tilt-X for left-handed use.
    ToggleTabletLeftHanded,
    /// Toggle proportional tablet active-area mapping (state stored; full
    /// implementation requires physical tablet aspect ratio from device info).
    ToggleTabletForceProportions,
    /// Show/hide the negotiated-truth session-info panel (w5-negotiated-truth):
    /// actual codec/chroma/depth/range/matrix, hardware-vs-software encode
    /// and decode, and any degradation from what settings asked for.
    ToggleNegotiatedTruth,
    /// Cycle which `arcen_media::test_pattern::TestPattern` the live
    /// pixel-exactness readout (w4-exactness-readout) compares the decoded
    /// frame against: None -> every pattern in turn -> None.
    #[cfg(feature = "dev-tools")]
    CycleTestPattern,
}

static MENU_COMMANDS: Mutex<Vec<MenuCommand>> = Mutex::new(Vec::new());

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn push_menu_command(command: MenuCommand) {
    if let Ok(mut queue) = MENU_COMMANDS.lock() {
        queue.push(command);
    }
}

/// Drain the menu commands queued since the previous frame. Called from the
/// egui app's `logic()` so menu clicks are applied on the UI thread.
pub fn drain_menu_commands() -> Vec<MenuCommand> {
    MENU_COMMANDS
        .lock()
        .map(|mut queue| std::mem::take(&mut *queue))
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{push_menu_command, MenuCommand};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, Sel};
    use objc2::sel;
    use objc2::{define_class, msg_send, AnyThread, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSEventModifierFlags, NSEventType, NSMenu,
        NSMenuItem,
    };
    use objc2_foundation::{MainThreadMarker, NSActivityOptions, NSProcessInfo, NSString};

    const APP_NAME: &str = "Arcen Deck";
    const VIEW_MENU_TITLE: &str = "View\u{200B}";

    define_class!(
        // Stateless AppKit target for the native menu items. Each action
        // method queues a `MenuCommand`; the egui app drains the queue. The
        // instance is leaked in `install()` because NSMenuItem holds its
        // target unretained and it must outlive the menu.
        #[unsafe(super(NSObject))]
        #[name = "ArcenMenuTarget"]
        struct MenuTarget;

        impl MenuTarget {
            #[unsafe(method(arcenQuit:))]
            fn quit(&self, _sender: *mut AnyObject) {
                // Asked here, while the event that fired the item is still
                // current: by the next frame a quick tap is already let go.
                let from_keyboard = MainThreadMarker::new().is_some_and(|mtm| {
                    NSApplication::sharedApplication(mtm)
                        .currentEvent()
                        .is_some_and(|event| event.r#type() == NSEventType::KeyDown)
                });
                push_menu_command(MenuCommand::Quit { from_keyboard });
            }

            #[unsafe(method(arcenToggleHealth:))]
            fn toggle_health(&self, _sender: *mut AnyObject) {
                push_menu_command(MenuCommand::ToggleHealth);
            }

            #[unsafe(method(arcenDisconnect:))]
            fn disconnect(&self, _sender: *mut AnyObject) {
                push_menu_command(MenuCommand::Disconnect);
            }

            #[unsafe(method(arcenReleaseModifiers:))]
            fn release_modifiers(&self, _sender: *mut AnyObject) {
                push_menu_command(MenuCommand::ReleaseModifiers);
            }

            #[unsafe(method(arcenToggleFullscreen:))]
            fn toggle_fullscreen(&self, _sender: *mut AnyObject) {
                push_menu_command(MenuCommand::ToggleFullscreen);
            }

            #[unsafe(method(arcenTogglePointerLock:))]
            fn toggle_pointer_lock(&self, _sender: *mut AnyObject) {
                push_menu_command(MenuCommand::TogglePointerLock);
            }

            #[unsafe(method(arcenToggleTabletMonitor:))]
            fn toggle_tablet_monitor(&self, _sender: *mut AnyObject) {
                push_menu_command(MenuCommand::ToggleTabletMonitor);
            }

            #[unsafe(method(arcenToggleTabletLeftHanded:))]
            fn toggle_tablet_left_handed(&self, _sender: *mut AnyObject) {
                push_menu_command(MenuCommand::ToggleTabletLeftHanded);
            }

            #[unsafe(method(arcenToggleTabletForceProportions:))]
            fn toggle_tablet_force_proportions(&self, _sender: *mut AnyObject) {
                push_menu_command(MenuCommand::ToggleTabletForceProportions);
            }

            #[unsafe(method(arcenToggleNegotiatedTruth:))]
            fn toggle_negotiated_truth(&self, _sender: *mut AnyObject) {
                push_menu_command(MenuCommand::ToggleNegotiatedTruth);
            }

            #[cfg(feature = "dev-tools")]
            #[unsafe(method(arcenCycleTestPattern:))]
            fn cycle_test_pattern(&self, _sender: *mut AnyObject) {
                push_menu_command(MenuCommand::CycleTestPattern);
            }
        }

        unsafe impl NSObjectProtocol for MenuTarget {}
    );

    impl MenuTarget {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            unsafe { msg_send![super(this), init] }
        }
    }

    struct KeyEquivalent {
        key: &'static str,
        modifiers: Option<NSEventModifierFlags>,
    }

    /// Keep macOS from App Nap-throttling the client while it streams.
    /// Without this a background/unfocused viewer degrades after ~a minute:
    /// repaint timers get coalesced, the receive drain slows, and wire age
    /// climbs to seconds (observed in soak testing). The returned activity
    /// token must live for the whole process — we intentionally leak it.
    pub fn disable_app_nap() {
        let info = NSProcessInfo::processInfo();
        let token = info.beginActivityWithOptions_reason(
            NSActivityOptions::UserInteractive | NSActivityOptions::LatencyCritical,
            &ns("Arcen remote desktop streaming"),
        );
        std::mem::forget(token);
    }

    thread_local! {
        /// The app-menu items whose shortcuts belong to the remote desktop
        /// while a session is on screen, with the key each normally carries.
        static HOST_SHORTCUT_ITEMS: std::cell::RefCell<Vec<(Retained<NSMenuItem>, &'static str)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Gives Cmd+H and Cmd+Option+H to the remote desktop, or back to this
    /// app. Main thread only; a no-op elsewhere.
    ///
    /// With them left on the menu, AppKit acts on them before the window sees
    /// the key, and hides the Deck instead of the application on the host.
    /// The menu items stay, so hiding is still a click. Cmd+Q is not here:
    /// it stays on Quit, which asks the app rather than terminating, so it
    /// can be hold-to-quit in a session.
    pub fn forward_app_shortcuts_to_host(forward: bool) {
        if MainThreadMarker::new().is_none() {
            return;
        }
        HOST_SHORTCUT_ITEMS.with(|items| {
            for (item, key) in items.borrow().iter() {
                item.setKeyEquivalent(&ns(if forward { "" } else { key }));
            }
        });
    }

    /// Whether Command and Q are both physically held right now.
    ///
    /// Read from the window server's combined state rather than from events:
    /// Cmd+Q is consumed by the menu, so its key-down never reaches the
    /// window, and the question each frame is simply whether it is still
    /// down.
    pub fn quit_chord_held() -> bool {
        command_held() && {
            // SAFETY: a plain query; 0 is the combined session state and 12
            // the Q key's virtual key code.
            unsafe { CGEventSourceKeyState(0, 12) }
        }
    }

    /// Whether Command is physically held right now.
    pub fn command_held() -> bool {
        const COMMAND: u64 = 1 << 20;
        // SAFETY: a plain query of the combined session state.
        unsafe { CGEventSourceFlagsState(0) & COMMAND != 0 }
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
        fn CGEventSourceFlagsState(state: i32) -> u64;
    }

    pub fn install() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };

        let app = NSApplication::sharedApplication(mtm);
        let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

        // One leaked target routes native menu clicks into the app's command
        // queue (NSMenuItem keeps its target unretained, so it must outlive
        // the menu — see the forget at the end of this function).
        let menu_target = MenuTarget::new();

        let menubar = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns(""));

        let app_menu = make_menu(mtm, APP_NAME);
        app_menu.addItem(&menu_item(
            mtm,
            &format!("About {APP_NAME}"),
            Some(sel!(orderFrontStandardAboutPanel:)),
            None,
        ));
        app_menu.addItem(&NSMenuItem::separatorItem(mtm));
        let services_menu = make_menu(mtm, "Services");
        let services_item = menu_item(mtm, "Services", None, None);
        services_item.setSubmenu(Some(&services_menu));
        app_menu.addItem(&services_item);
        app_menu.addItem(&NSMenuItem::separatorItem(mtm));
        let hide = menu_item(
            mtm,
            &format!("Hide {APP_NAME}"),
            Some(sel!(hide:)),
            Some(KeyEquivalent {
                key: "h",
                modifiers: None,
            }),
        );
        app_menu.addItem(&hide);
        let hide_others = menu_item(
            mtm,
            "Hide Others",
            Some(sel!(hideOtherApplications:)),
            Some(KeyEquivalent {
                key: "h",
                modifiers: Some(NSEventModifierFlags::Option | NSEventModifierFlags::Command),
            }),
        );
        app_menu.addItem(&hide_others);
        app_menu.addItem(&menu_item(
            mtm,
            "Show All",
            Some(sel!(unhideAllApplications:)),
            None,
        ));
        app_menu.addItem(&NSMenuItem::separatorItem(mtm));
        // Not `terminate:`: the app decides. Outside a session it quits at
        // once; in one, Cmd+Q is Chrome's hold-to-quit, and a tap goes to the
        // host. A menu key equivalent is matched before the window ever sees
        // the key, so this item is what receives every Cmd+Q.
        app_menu.addItem(&action_item(
            mtm,
            &menu_target,
            &format!("Quit {APP_NAME}"),
            sel!(arcenQuit:),
            Some(KeyEquivalent {
                key: "q",
                modifiers: None,
            }),
        ));
        HOST_SHORTCUT_ITEMS.with(|items| {
            *items.borrow_mut() = vec![(hide, "h"), (hide_others, "h")];
        });
        add_submenu(mtm, &menubar, APP_NAME, &app_menu);

        let connection_menu = make_menu(mtm, "Connection");
        connection_menu.addItem(&action_item(
            mtm,
            &menu_target,
            "Disconnect",
            sel!(arcenDisconnect:),
            Some(KeyEquivalent {
                key: "\u{F70F}",
                modifiers: Some(NSEventModifierFlags::Control | NSEventModifierFlags::Option),
            }),
        ));
        connection_menu.addItem(&action_item(
            mtm,
            &menu_target,
            "Release Modifiers",
            sel!(arcenReleaseModifiers:),
            None,
        ));
        add_submenu(mtm, &menubar, "Connection", &connection_menu);

        let view_menu = make_menu(mtm, VIEW_MENU_TITLE);
        view_menu.addItem(&action_item(
            mtm,
            &menu_target,
            "Show Fullscreen",
            sel!(arcenToggleFullscreen:),
            Some(KeyEquivalent {
                key: "f",
                modifiers: Some(NSEventModifierFlags::Control | NSEventModifierFlags::Command),
            }),
        ));
        view_menu.addItem(&action_item(
            mtm,
            &menu_target,
            "Pointer Lock",
            sel!(arcenTogglePointerLock:),
            None,
        ));
        view_menu.addItem(&action_item(
            mtm,
            &menu_target,
            "Tablet Monitor",
            sel!(arcenToggleTabletMonitor:),
            None,
        ));
        view_menu.addItem(&action_item(
            mtm,
            &menu_target,
            "Tablet Orientation Left-handed",
            sel!(arcenToggleTabletLeftHanded:),
            None,
        ));
        add_submenu(mtm, &menubar, VIEW_MENU_TITLE, &view_menu);

        // "Health" and "Colour" were separate top-level menus holding one item
        // each. Both answer the same question -- what is this session actually
        // doing right now -- so they live together, and the menu bar loses two
        // single-item menus.
        let session_menu = make_menu(mtm, "Session");
        session_menu.addItem(&action_item(
            mtm,
            &menu_target,
            "Connection Health",
            sel!(arcenToggleHealth:),
            None,
        ));
        session_menu.addItem(&action_item(
            mtm,
            &menu_target,
            "Negotiated Truth",
            sel!(arcenToggleNegotiatedTruth:),
            None,
        ));
        #[cfg(feature = "dev-tools")]
        session_menu.addItem(&action_item(
            mtm,
            &menu_target,
            "Cycle Test Pattern Readout",
            sel!(arcenCycleTestPattern:),
            None,
        ));
        add_submenu(mtm, &menubar, "Session", &session_menu);

        app.setServicesMenu(Some(&services_menu));
        app.setMainMenu(Some(&menubar));
        remove_items(
            &view_menu,
            &["Show Tab Bar", "Show All Tabs", "Enter Full Screen"],
        );
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);

        // NSMenuItem does not retain its target, so the menu target must live
        // for the whole process. Intentionally leak it (matching the app-nap
        // token) rather than tie it to this function's stack frame.
        std::mem::forget(menu_target);
    }

    fn make_menu(mtm: MainThreadMarker, title: &str) -> Retained<NSMenu> {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns(title));
        menu.setAutoenablesItems(false);
        menu
    }

    fn remove_items(menu: &NSMenu, titles: &[&str]) {
        let mut index = menu.numberOfItems();
        while index > 0 {
            index -= 1;
            if let Some(item) = menu.itemAtIndex(index) {
                let title = item.title().to_string();
                if titles.contains(&title.as_str()) {
                    menu.removeItemAtIndex(index);
                }
            }
        }
    }

    fn add_submenu(mtm: MainThreadMarker, menubar: &NSMenu, title: &str, submenu: &NSMenu) {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&ns(title));
        item.setSubmenu(Some(submenu));
        menubar.addItem(&item);
    }

    /// A menu item wired to `target`/`selector` so clicking it queues a
    /// `MenuCommand` the egui app applies on the UI thread.
    fn action_item(
        mtm: MainThreadMarker,
        target: &MenuTarget,
        title: &str,
        selector: Sel,
        key_equivalent: Option<KeyEquivalent>,
    ) -> Retained<NSMenuItem> {
        let item = menu_item(mtm, title, Some(selector), key_equivalent);
        unsafe { item.setTarget(Some(target)) };
        item.setEnabled(true);
        item
    }

    fn menu_item(
        mtm: MainThreadMarker,
        title: &str,
        selector: Option<Sel>,
        key_equivalent: Option<KeyEquivalent>,
    ) -> Retained<NSMenuItem> {
        let key = ns(key_equivalent
            .as_ref()
            .map(|equivalent| equivalent.key)
            .unwrap_or(""));
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &ns(title),
                selector,
                &key,
            )
        };
        if let Some(modifiers) = key_equivalent.and_then(|equivalent| equivalent.modifiers) {
            item.setKeyEquivalentModifierMask(modifiers);
        }
        item
    }

    fn ns(value: &str) -> Retained<NSString> {
        NSString::from_str(value)
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub fn install() {}
    pub fn disable_app_nap() {}
    pub fn forward_app_shortcuts_to_host(_forward: bool) {}
    pub fn quit_chord_held() -> bool {
        false
    }
    pub fn command_held() -> bool {
        false
    }
}

pub fn install() {
    platform::install();
}

/// See `platform::quit_chord_held`.
pub fn quit_chord_held() -> bool {
    platform::quit_chord_held()
}

/// See `platform::command_held`.
pub fn command_held() -> bool {
    platform::command_held()
}

/// See `platform::forward_app_shortcuts_to_host`.
pub fn forward_app_shortcuts_to_host(forward: bool) {
    platform::forward_app_shortcuts_to_host(forward);
}

pub fn disable_app_nap() {
    platform::disable_app_nap();
}

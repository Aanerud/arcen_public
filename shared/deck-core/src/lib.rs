// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The portable Arcen Deck.
//!
//! A Deck is the client that shows a Pier's desktop. Everything a Deck does
//! that an operating system does not force on it lives here, so the macOS
//! Deck (`clients/macos`) and the Windows Deck (`clients/windows`) are thin
//! shells around one implementation (ADR 0024, root `AGENTS.md`).
//!
//! # The seams
//!
//! A platform shell implements three traits and nothing else crosses the
//! boundary:
//!
//! - [`DeckServices`]: what the session engine asks of the operating system
//!   (clipboard, decode capabilities, microphone, network facts, pen
//!   capture, build identity, config and log directories, displays and
//!   their colour facts, time zone).
//! - [`decode::VideoDecoderBackend`] (with [`decode::VideoDecoderFactory`]):
//!   one hardware decoder per monitor, producing a native picture the
//!   platform presenter can sample without a copy.
//! - [`present::VideoPresenterSink`]: the dedicated eight-bit and ten-bit
//!   video presenters, including the frame-exact Exact still image fences
//!   ([`arcen_media::refine::hub::RefineCompositor`]).
//!
//! # Features
//!
//! | Feature | Adds | Graph |
//! | --- | --- | --- |
//! | default | the seams, settings and pure Deck logic | std, serde, zeroize, getrandom, tracing and shared crates (TLS-only transport) |
//! | `runtime` | session engine, lanes, media worker, frame queue, [`runtime::MicrophoneQueue`] | tokio, `arcen-transport/quic` |
//! | `egui` | the shared Deck screens (`screens`: Home, Settings, connection test, theme) | egui (no eframe, no wgpu) |
//! | `wgpu-presenter` | the portable wgpu video paint callback | egui-wgpu, wgpu |
//! | `support-bundle` | the support bundle writer ([`support_bundle::create`]) | zip (deflate) |
//!
//! CI proves the default graph contains no tokio, quinn, egui, wgpu or
//! product package; a shell that only needs settings never pulls a runtime.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::items_after_statements,
        clippy::needless_pass_by_value,
        clippy::unchecked_time_subtraction,
        clippy::cast_possible_truncation
    )
)]

pub mod about;
pub mod client_telemetry;
pub mod clipboard;
pub mod config;
#[cfg(feature = "runtime")]
pub mod connect_plan;
pub mod connection_draft;
pub mod decode;
pub mod decode_report;
pub mod directories;
pub mod display;
pub mod display_fit;
#[cfg(feature = "runtime")]
pub mod display_follow;
pub mod display_note;
pub mod input;
pub mod installed_copies;
pub mod keymap;
pub mod log_target;
#[cfg(feature = "runtime")]
pub mod logging;
pub mod microphone;
pub mod multi_window;
pub mod network;
pub mod password_text;
pub mod paste_keys;
pub mod present;
pub mod quick_connect;
pub mod reconnect;
#[cfg(feature = "runtime")]
pub mod reconnect_flow;
pub mod services;
pub mod session_truth;
pub mod settings;
pub mod support_bundle;
pub mod trusted_pins;
pub mod ui_pacing;
pub mod video_stall;

#[cfg(feature = "runtime")]
pub mod runtime;

#[cfg(feature = "egui")]
pub mod screens;

#[cfg(feature = "wgpu-presenter")]
pub mod wgpu_presenter;

pub use services::{DeckServices, typed_pen_capability};

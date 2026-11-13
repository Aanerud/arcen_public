// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Notices a desktop's logind session ending outside Arcen.
//!
//! An administrator's `loginctl terminate-session`, or anything else that
//! ends the session, leaves the Pier holding a retained desktop whose
//! processes are gone. Before this watch the desktop, and a native desktop's
//! own seat with it, stayed until the next sign-in found it dead or the Pier
//! restarted. logind announces the end with `SessionRemoved`.

use futures_util::StreamExt as _;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, Proxy};

const LOGIND: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const LOGIND_MANAGER: &str = "org.freedesktop.login1.Manager";
const NO_SUCH_SESSION: &str = "org.freedesktop.login1.NoSuchSession";

/// Resolves once logind no longer has session `session_id`.
///
/// Subscribes first and asks second, so a session that ends between the
/// two is still seen.
///
/// # Errors
///
/// When the system bus or logind cannot be reached, or the signal stream
/// ends; the caller then keeps the lazy check at the next sign-in.
pub async fn session_removed(session_id: &str) -> zbus::Result<()> {
    let connection = Connection::system().await?;
    let manager = Proxy::new(&connection, LOGIND, LOGIND_PATH, LOGIND_MANAGER).await?;
    let mut removed = manager
        .receive_signal_with_args("SessionRemoved", &[(0, session_id)])
        .await?;
    match manager
        .call::<_, _, OwnedObjectPath>("GetSession", &(session_id,))
        .await
    {
        Ok(_) => {}
        Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == NO_SUCH_SESSION => {
            return Ok(());
        }
        Err(error) => return Err(error),
    }
    while let Some(message) = removed.next().await {
        let removed_id = message
            .body()
            .deserialize::<(String, OwnedObjectPath)>()
            .map(|(id, _path)| id);
        if removed_id.as_deref() == Ok(session_id) {
            return Ok(());
        }
    }
    Err(zbus::Error::Failure(
        "logind SessionRemoved stream ended".to_string(),
    ))
}

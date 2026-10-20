//! The UID of the qualification's temporary local account.
//!
//! logind keys its users by UID. A qualification that was killed can leave
//! its account's logind user stuck in `closing` (seen with systemd 257: no
//! processes, never collected until `systemd-logind` restarts). Deleting the
//! account frees its name but not that UID, and `useradd` would hand the same
//! UID to the next account, whose sign-in then meets the stale user. So the
//! next account gets a UID nothing on the host still holds, chosen the way
//! `useradd` chooses: above the highest local account in the login range,
//! never a gap below it, where a deleted user's files may still be owned.

use std::ops::RangeInclusive;

/// `UID_MIN..=UID_MAX` of `useradd` when `login.defs` does not say.
pub const DEFAULT_LOGIN_UID_RANGE: RangeInclusive<u32> = 1_000..=60_000;

/// The login UID range of a `login.defs` text.
#[must_use]
pub fn login_uid_range(login_defs: &str) -> RangeInclusive<u32> {
    let value = |key: &str| {
        login_defs.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some(key))
                .then(|| fields.next()?.parse::<u32>().ok())
                .flatten()
        })
    };
    let min = value("UID_MIN").unwrap_or(*DEFAULT_LOGIN_UID_RANGE.start());
    let max = value("UID_MAX").unwrap_or(*DEFAULT_LOGIN_UID_RANGE.end());
    if min <= max {
        min..=max
    } else {
        DEFAULT_LOGIN_UID_RANGE
    }
}

/// The UID for a new temporary account: one above the highest UID of a
/// local account (`passwd`, `/etc/passwd` text) in `range`, then upwards past
/// every UID `held` reports (a logind user or runtime directory left
/// behind, an account from a directory service). `None` when the range has
/// no such UID left.
#[must_use]
pub fn temporary_account_uid(
    passwd: &str,
    range: &RangeInclusive<u32>,
    held: impl Fn(u32) -> bool,
) -> Option<u32> {
    let highest = passwd
        .lines()
        .filter_map(|line| line.split(':').nth(2)?.parse::<u32>().ok())
        .filter(|uid| range.contains(uid))
        .max();
    let first = highest.map_or(Some(*range.start()), |uid| uid.checked_add(1))?;
    (first..=*range.end()).find(|&uid| !held(uid))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
        nobody:x:65534:65534:Kernel Overflow User:/:/sbin/nologin\n\
        admin:x:1000:1000::/home/admin:/bin/bash\n\
        arcen-wlq:x:2101:2101:Arcen Wayland qualification (temporary):/home/arcen-wlq:/sbin/nologin\n\
        svc:x:998:996::/:/sbin/nologin\n";

    #[test]
    fn the_uid_follows_useradd_and_skips_what_logind_still_holds() {
        let range = DEFAULT_LOGIN_UID_RANGE;
        assert_eq!(temporary_account_uid(PASSWD, &range, |_| false), Some(2102));
        // A killed run's logind user 2102 is still closing: the next is used.
        assert_eq!(
            temporary_account_uid(PASSWD, &range, |uid| uid == 2102 || uid == 2103),
            Some(2104)
        );
        // Never a gap below the highest account (a deleted user's files).
        assert_eq!(
            temporary_account_uid(
                "a:x:1000:1000::/:/bin/sh\nb:x:1005:1005::/:/bin/sh\n",
                &range,
                |_| false
            ),
            Some(1006)
        );
        // No local account in the range: its first UID.
        assert_eq!(
            temporary_account_uid("root:x:0:0::/:/bin/sh\n", &range, |_| false),
            Some(1000)
        );
        // A full range has none to give.
        assert_eq!(
            temporary_account_uid(PASSWD, &(1_000..=2_101), |_| false),
            None
        );
        assert_eq!(
            temporary_account_uid(PASSWD, &(1_000..=2_103), |uid| uid > 2_101),
            None
        );
    }

    #[test]
    fn the_login_range_is_read_from_login_defs() {
        let defs = "# comment\nUID_MIN                  1000\nUID_MAX   60000\nSYS_UID_MIN 201\n";
        assert_eq!(login_uid_range(defs), 1_000..=60_000);
        assert_eq!(login_uid_range("UID_MIN 5000\n"), 5_000..=60_000);
        assert_eq!(login_uid_range(""), DEFAULT_LOGIN_UID_RANGE);
        assert_eq!(
            login_uid_range("UID_MIN 9\nUID_MAX 3\n"),
            DEFAULT_LOGIN_UID_RANGE
        );
        assert_eq!(login_uid_range("UID_MIN nope\n"), DEFAULT_LOGIN_UID_RANGE);
    }
}

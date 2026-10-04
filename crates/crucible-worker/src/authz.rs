//! Who may do what. Pure functions so the rules are tested on their own.

use crate::http::ApiError;

/// An authenticated, non-banned caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub github_id: u64,
    pub login: String,
    pub is_admin: bool,
}

/// Eval details, results and downloads: the owner or an admin.
pub fn can_view_eval(p: &Principal, owner_id: u64) -> bool {
    p.is_admin || p.github_id == owner_id
}

/// Admin-only endpoints.
pub fn require_admin(p: &Principal) -> Result<(), ApiError> {
    if p.is_admin {
        Ok(())
    } else {
        Err(ApiError::forbidden("administrators only"))
    }
}

/// Admins are configured in the environment; banning one would be
/// ineffective-looking and confusing, so it is refused outright.
pub fn check_ban_target(
    actor: &Principal,
    target: u64,
    target_is_admin: bool,
) -> Result<(), ApiError> {
    require_admin(actor)?;
    if target_is_admin {
        return Err(ApiError::bad_request("cannot ban an administrator"));
    }
    if target == 0 {
        return Err(ApiError::bad_request(
            "github_id must be a positive integer",
        ));
    }
    Ok(())
}

/// A ban outranks everything, admin status included (an admin whose id is
/// later banned by a direct database edit is locked out too).
pub fn admit(
    github_id: u64,
    login: &str,
    banned: bool,
    is_admin: bool,
) -> Result<Principal, ApiError> {
    if banned {
        return Err(ApiError::banned());
    }
    Ok(Principal {
        github_id,
        login: login.to_owned(),
        is_admin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: u64, admin: bool) -> Principal {
        Principal {
            github_id: id,
            login: format!("u{id}"),
            is_admin: admin,
        }
    }

    #[test]
    fn view_rules() {
        assert!(can_view_eval(&user(7, false), 7));
        assert!(!can_view_eval(&user(8, false), 7));
        assert!(can_view_eval(&user(1, true), 7));
    }

    #[test]
    fn ban_rules() {
        assert_eq!(
            check_ban_target(&user(7, false), 8, false)
                .unwrap_err()
                .code,
            "forbidden"
        );
        assert!(check_ban_target(&user(1, true), 8, false).is_ok());
        assert!(check_ban_target(&user(1, true), 2, true).is_err());
        assert!(check_ban_target(&user(1, true), 0, false).is_err());
    }

    #[test]
    fn banned_users_are_refused() {
        assert_eq!(admit(7, "x", true, false).unwrap_err().code, "banned");
        assert_eq!(admit(1, "x", true, true).unwrap_err().code, "banned");
        let p = admit(1, "x", false, true).unwrap();
        assert!(p.is_admin);
        assert!(require_admin(&admit(7, "x", false, false).unwrap()).is_err());
    }
}

use crate::errors::AppError;

/// Validate password complexity:
/// - at least 8 characters
/// - contains at least one uppercase letter
/// - contains at least one lowercase letter
/// - contains at least one digit
pub fn validate_password(password: &str) -> Result<(), AppError> {
    if password.len() < 8 {
        return Err(AppError::BadRequest(
            "Password must be at least 8 characters".into(),
        ));
    }
    let has_upper = password.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = password.chars().any(|c| c.is_ascii_lowercase());
    let has_digit = password.chars().any(|c| c.is_ascii_digit());
    if !has_upper || !has_lower || !has_digit {
        return Err(AppError::BadRequest(
            "Password must contain at least one uppercase letter, one lowercase letter, and one digit".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_password() {
        assert!(validate_password("Abcdef1x").is_ok());
    }

    #[test]
    fn too_short() {
        assert!(validate_password("Ab1").is_err());
    }

    #[test]
    fn no_uppercase() {
        assert!(validate_password("abcdef12").is_err());
    }

    #[test]
    fn no_lowercase() {
        assert!(validate_password("ABCDEF12").is_err());
    }

    #[test]
    fn no_digit() {
        assert!(validate_password("Abcdefgh").is_err());
    }
}

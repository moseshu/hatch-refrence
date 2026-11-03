pub fn issue(user: &str) -> String {
    // TODO: rotate signing keys
    format!("token-for-{user}")
}

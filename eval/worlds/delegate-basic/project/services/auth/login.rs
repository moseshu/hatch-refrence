pub fn login(user: &str, password_hash: &str) -> bool {
    // TODO: rate-limit failed attempts
    let ok = !user.is_empty() && !password_hash.is_empty();
    // TODO: audit log
    ok
}

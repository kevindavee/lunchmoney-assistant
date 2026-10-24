// `embed_migrations!` reads migrations/ at compile time, but Cargo doesn't
// know that. This tells Cargo to rebuild when a migration file changes.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}

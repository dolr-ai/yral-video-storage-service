//! Makes cargo rebuild when a migration changes.
//!
//! `refinery::embed_migrations!` reads `migrations/` at macro-expansion time but
//! does not register those files as build dependencies. Without this, editing or
//! adding a `.sql` file leaves the previously-embedded set compiled in: the new
//! migration simply never runs, and tests fail with "relation does not exist"
//! pointing at a file that plainly contains the CREATE. Cost us one debugging
//! round already — do not remove.
fn main() {
    println!("cargo::rerun-if-changed=migrations");
}

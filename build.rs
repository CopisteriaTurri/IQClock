//! Icona e metadati dell'eseguibile Windows.
//!
//! L'icona della finestra (impostata a runtime, dalla stessa immagine PNG) non
//! finisce nel file `.exe`: Esplora risorse mostra l'icona incorporata nel PE e
//! le proprietà del file (versione, descrizione). Qui le aggiungiamo con
//! `winresource`, solo quando il target è Windows.

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=assets/iqclock.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let versione = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    // Windows vuole quattro numeri: 0.1.0 → 0.1.0.0.
    let versione = match versione.split('.').count() {
        3 => format!("{versione}.0"),
        _ => versione,
    };
    winresource::WindowsResource::new()
        .set_icon("assets/iqclock.ico")
        .set("ProductName", "IQClock")
        .set("FileDescription", "IQClock: stampa su DesignJet PostScript")
        .set("LegalCopyright", "GPL-3.0-only")
        .set("FileVersion", versione.as_str())
        .set("ProductVersion", versione.as_str())
        .compile()
        .expect("incorporo icona e metadati nell'exe Windows");
}

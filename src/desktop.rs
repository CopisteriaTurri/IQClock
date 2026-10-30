//! Icona su Linux. Sotto Wayland la finestra non può dare la sua icona: GNOME
//! e KDE prendono l'app_id, cercano il file `.desktop` con lo stesso nome e da
//! lì l'icona nel tema. Senza, si vede un'icona generica.
//!
//! Non c'è un installer, quindi all'avvio scriviamo noi in `~/.local/share`
//! il file `.desktop` (che punta a questo eseguibile) e l'icona, solo se
//! mancano o sono cambiati. Così l'app compare anche nel menu applicazioni.

use std::io;
use std::path::{Path, PathBuf};

pub fn installa(icona_png: &[u8]) -> io::Result<()> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or_else(|| io::Error::other("cartella dati utente non trovata"))?;
    scrivi_se_diverso(&base.join("icons/hicolor/256x256/apps/iqclock.png"), icona_png)?;
    let exe = std::env::current_exe()?;
    scrivi_se_diverso(&base.join("applications/iqclock.desktop"), voce_desktop(&exe).as_bytes())
}

fn voce_desktop(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=IQClock\n\
         Comment=Stampa su HP DesignJet PostScript\n\
         Exec={} %F\n\
         Icon=iqclock\n\
         StartupWMClass=iqclock\n\
         Terminal=false\n\
         Categories=Graphics;Office;\n\
         MimeType=application/pdf;application/postscript;image/png;image/jpeg;image/tiff;\n",
        argomento_exec(exe)
    )
}

/// Percorso tra virgolette con gli escape della specifica Desktop Entry: i
/// percorsi con spazi o caratteri speciali restano un argomento solo.
fn argomento_exec(exe: &Path) -> String {
    let mut s = String::from("\"");
    for c in exe.to_string_lossy().chars() {
        match c {
            '"' | '`' | '$' | '\\' => s.push_str(&format!("\\\\{c}")),
            '%' => s.push_str("%%"),
            _ => s.push(c),
        }
    }
    s.push('"');
    s
}

fn scrivi_se_diverso(percorso: &Path, contenuto: &[u8]) -> io::Result<()> {
    if std::fs::read(percorso).ok().as_deref() == Some(contenuto) {
        return Ok(());
    }
    if let Some(dir) = percorso.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(percorso, contenuto)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn exec_con_spazi_e_caratteri_speciali() {
        assert_eq!(argomento_exec(Path::new("/home/a b/iqclock")), "\"/home/a b/iqclock\"");
        assert_eq!(argomento_exec(Path::new("/x/$y%z")), "\"/x/\\\\$y%%z\"");
    }
}

//! Impostazioni persistenti in un file `chiave=valore`
//! (`%APPDATA%\iqclock\config.txt` su Windows, `~/.config/iqclock/config.txt` altrove).

use std::path::PathBuf;

use crate::rotazione::Rotazione;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub porta: u16,
    pub larghezza_rotolo_mm: f64,
    pub rotazione: Rotazione,
}

impl Default for Config {
    fn default() -> Self {
        // IP della T2600 nei log di HP Click.
        Self { host: "192.168.10.72".into(), porta: 9100, larghezza_rotolo_mm: 914.0, rotazione: Rotazione::Auto }
    }
}

fn percorso() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    })?;
    Some(base.join("iqclock").join("config.txt"))
}

impl Config {
    pub fn carica() -> Self {
        let mut c = Config::default();
        let Some(testo) = percorso().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return c;
        };
        for (k, v) in testo.lines().filter_map(|r| r.split_once('=')) {
            match k.trim() {
                "host" => c.host = v.trim().to_string(),
                "porta" => c.porta = v.trim().parse().unwrap_or(c.porta),
                "rotolo_mm" => c.larghezza_rotolo_mm = v.trim().parse().unwrap_or(c.larghezza_rotolo_mm),
                "rotazione" => c.rotazione = Rotazione::parse(v).unwrap_or(c.rotazione),
                _ => {}
            }
        }
        c
    }

    pub fn salva(&self) {
        let Some(p) = percorso() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(
            p,
            format!(
                "host={}\nporta={}\nrotolo_mm={}\nrotazione={}\n",
                self.host,
                self.porta,
                self.larghezza_rotolo_mm,
                match self.rotazione {
                    Rotazione::Auto => "auto",
                    Rotazione::Si => "si",
                    Rotazione::No => "no",
                },
            ),
        );
    }
}

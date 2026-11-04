//! Impostazioni persistenti in un file `chiave=valore`
//! (`%APPDATA%\iqclock\config.txt` su Windows, `~/.config/iqclock/config.txt` altrove).

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub host: String,
    pub porta: u16,
    pub larghezza_rotolo_mm: f64,
    /// Bordo non stampabile per lato, usato per allineare al centro o a destra.
    pub margine_mm: f64,
    /// Sotto questi DPI effettivi miniature e anteprima avvisano (la stampa non
    /// viene bloccata).
    pub soglia_dpi: f64,
}

impl Default for Config {
    fn default() -> Self {
        // IP della T2600 nei log di HP Click.
        Self { host: "192.168.10.72".into(), porta: 9100, larghezza_rotolo_mm: 914.0, margine_mm: 5.0, soglia_dpi: 200.0 }
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
        let Some(p) = percorso() else { return Config::default() };
        Self::carica_da(&p)
    }

    pub fn salva(&self) {
        let Some(p) = percorso() else { return };
        self.salva_su(&p);
    }

    fn carica_da(p: &Path) -> Self {
        let mut c = Config::default();
        let Ok(testo) = std::fs::read_to_string(p) else { return c };
        for (k, v) in testo.lines().filter_map(|r| r.split_once('=')) {
            match k.trim() {
                "host" => c.host = v.trim().to_string(),
                "porta" => c.porta = v.trim().parse().unwrap_or(c.porta),
                "rotolo_mm" => c.larghezza_rotolo_mm = v.trim().parse().unwrap_or(c.larghezza_rotolo_mm),
                "margine_mm" => c.margine_mm = v.trim().parse().unwrap_or(c.margine_mm),
                "soglia_dpi" => {
                    // Una soglia nulla o negativa avviserebbe sempre: vale il default.
                    if let Ok(soglia) = v.trim().parse::<f64>()
                        && soglia.is_finite()
                        && soglia > 0.0
                    {
                        c.soglia_dpi = soglia;
                    }
                }
                _ => {}
            }
        }
        c
    }

    fn salva_su(&self, p: &Path) {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(
            p,
            format!(
                "host={}\nporta={}\nrotolo_mm={}\nmargine_mm={}\nsoglia_dpi={}\n",
                self.host, self.porta, self.larghezza_rotolo_mm, self.margine_mm, self.soglia_dpi
            ),
        );
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Percorso isolato per test, mai la config reale.
    fn temporanea(nome: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("iqclock-config-test-{}-{nome}", std::process::id()))
    }

    #[test]
    fn file_mancante_da_default() {
        let p = temporanea("mancante");
        let _ = std::fs::remove_file(&p);
        let c = Config::carica_da(&p);
        assert_eq!(c, Config::default());
    }

    #[test]
    fn valori_corrotti_tengono_i_default() {
        let p = temporanea("corrotta");
        std::fs::write(&p, "spazzatura senza uguale\nhost = 10.0.0.5\nporta = abc\nrotolo_mm =\nchiave_ignota=1\n").unwrap();
        let c = Config::carica_da(&p);
        std::fs::remove_file(&p).unwrap();
        let d = Config::default();
        assert_eq!(c.host, "10.0.0.5");
        assert_eq!(c.porta, d.porta);
        assert_eq!(c.larghezza_rotolo_mm, d.larghezza_rotolo_mm);
    }

    #[test]
    fn round_trip() {
        let p = temporanea("roundtrip");
        let c = Config { host: "stampante.lan".into(), porta: 9101, larghezza_rotolo_mm: 610.0, margine_mm: 3.0, soglia_dpi: 150.0 };
        c.salva_su(&p);
        let riletta = Config::carica_da(&p);
        std::fs::remove_file(&p).unwrap();
        assert_eq!(riletta, c);
    }

    #[test]
    fn soglia_dpi_di_default_e_valori_assurdi() {
        assert_eq!(Config::default().soglia_dpi, 200.0);
        let p = temporanea("soglia");
        std::fs::write(&p, "soglia_dpi=150\n").unwrap();
        assert_eq!(Config::carica_da(&p).soglia_dpi, 150.0);
        for assurda in ["soglia_dpi=0\n", "soglia_dpi=-20\n", "soglia_dpi=molta\n"] {
            std::fs::write(&p, assurda).unwrap();
            assert_eq!(Config::carica_da(&p).soglia_dpi, 200.0, "{assurda:?}");
        }
        std::fs::remove_file(&p).unwrap();
    }
}

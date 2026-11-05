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
    /// Cornice di taglio: distanza della linea dal contenuto e suo spessore.
    pub cornice_distanza_mm: f64,
    pub cornice_spessore_mm: f64,
    /// Nesting: attivo, se i pezzi si possono ruotare e lo spazio tra loro.
    pub nesting: bool,
    pub nesting_ruota: bool,
    pub nesting_distanza_mm: f64,
    /// Dimensione della finestra all'apertura, in pixel logici: alta abbastanza
    /// da mostrare tutte le sezioni del pannello senza scorrere.
    pub finestra_larghezza_px: u32,
    pub finestra_altezza_px: u32,
}

impl Default for Config {
    /// Impostazioni iniziali della stampante, della cornice e della finestra.
    fn default() -> Self {
        // IP della T2600 nei log di HP Click.
        Self {
            host: "192.168.10.72".into(),
            porta: 9100,
            larghezza_rotolo_mm: 914.0,
            margine_mm: 5.0,
            soglia_dpi: 200.0,
            cornice_distanza_mm: 3.0,
            cornice_spessore_mm: 0.25,
            nesting: false,
            nesting_ruota: true,
            nesting_distanza_mm: 5.0,
            finestra_larghezza_px: 1200,
            finestra_altezza_px: 1000,
        }
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

/// Valore booleano di una chiave: "1", "true" e "sì" accendono, il resto spegne.
fn booleano(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "si" | "sì")
}

/// Pixel di una misura della finestra: fuori intervallo vale il default, che
/// tiene la finestra usabile anche su schermi piccoli.
fn px_accettabili(v: &str, min: u32, max: u32) -> Option<u32> {
    v.trim().parse::<u32>().ok().filter(|px| (min..=max).contains(px))
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

    /// Legge le coppie chiave=valore; file assente o campi non validi mantengono
    /// i valori predefiniti. Ignora le chiavi sconosciute.
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
                "cornice_distanza_mm" => {
                    if let Ok(mm) = v.trim().parse::<f64>()
                        && mm.is_finite()
                        && (0.0..=100.0).contains(&mm)
                    {
                        c.cornice_distanza_mm = mm;
                    }
                }
                "cornice_spessore_mm" => {
                    // Una linea senza spessore non si vedrebbe: vale il default.
                    if let Ok(mm) = v.trim().parse::<f64>()
                        && mm.is_finite()
                        && (0.1..=10.0).contains(&mm)
                    {
                        c.cornice_spessore_mm = mm;
                    }
                }
                "nesting" => c.nesting = booleano(v),
                "nesting_ruota" => c.nesting_ruota = booleano(v),
                "nesting_distanza_mm" => {
                    if let Ok(mm) = v.trim().parse::<f64>()
                        && mm.is_finite()
                        && (0.0..=100.0).contains(&mm)
                    {
                        c.nesting_distanza_mm = mm;
                    }
                }
                "finestra_larghezza_px" => {
                    if let Some(px) = px_accettabili(v, 800, 4000) {
                        c.finestra_larghezza_px = px;
                    }
                }
                "finestra_altezza_px" => {
                    if let Some(px) = px_accettabili(v, 500, 4000) {
                        c.finestra_altezza_px = px;
                    }
                }
                _ => {}
            }
        }
        c
    }

    /// Salva tutte le impostazioni creando la cartella se necessario.
    /// Gli errori di scrittura non interrompono l’applicazione.
    fn salva_su(&self, p: &Path) {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(
            p,
            format!(
                "host={}\nporta={}\nrotolo_mm={}\nmargine_mm={}\nsoglia_dpi={}\ncornice_distanza_mm={}\ncornice_spessore_mm={}\nnesting={}\nnesting_ruota={}\nnesting_distanza_mm={}\nfinestra_larghezza_px={}\nfinestra_altezza_px={}\n",
                self.host, self.porta, self.larghezza_rotolo_mm, self.margine_mm, self.soglia_dpi,
                self.cornice_distanza_mm, self.cornice_spessore_mm, self.nesting, self.nesting_ruota,
                self.nesting_distanza_mm, self.finestra_larghezza_px, self.finestra_altezza_px
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

    /// Verifica che salvataggio e lettura conservino anche cornice e dimensioni
    /// della finestra personalizzate.
    #[test]
    fn round_trip() {
        let p = temporanea("roundtrip");
        let c = Config {
            host: "stampante.lan".into(),
            porta: 9101,
            larghezza_rotolo_mm: 610.0,
            margine_mm: 3.0,
            soglia_dpi: 150.0,
            cornice_distanza_mm: 10.0,
            cornice_spessore_mm: 0.5,
            nesting: true,
            nesting_ruota: false,
            nesting_distanza_mm: 8.0,
            finestra_larghezza_px: 1400,
            finestra_altezza_px: 900,
        };
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

    /// Verifica i valori iniziali della cornice e il ripristino dei default per
    /// misure della cornice o della finestra non valide.
    #[test]
    fn cornice_di_default_e_valori_assurdi() {
        let d = Config::default();
        assert_eq!((d.cornice_distanza_mm, d.cornice_spessore_mm), (3.0, 0.25));
        let p = temporanea("cornice");
        std::fs::write(&p, "cornice_distanza_mm=10\ncornice_spessore_mm=0.5\n").unwrap();
        let c = Config::carica_da(&p);
        assert_eq!((c.cornice_distanza_mm, c.cornice_spessore_mm), (10.0, 0.5));
        for assurda in [
            "cornice_distanza_mm=-1\n",
            "cornice_spessore_mm=0\n",
            "cornice_spessore_mm=dritta\n",
            "finestra_altezza_px=10\n",
            "finestra_larghezza_px=larga\n",
        ] {
            std::fs::write(&p, assurda).unwrap();
            assert_eq!(Config::carica_da(&p), Config::default(), "{assurda:?}");
        }
        std::fs::remove_file(&p).unwrap();
    }
}

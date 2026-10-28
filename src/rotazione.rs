//! Rotazione automatica di 90° per risparmiare carta sul rotolo.
//!
//! La stampante riceve i file così come sono e decide lei se ruotarli
//! (`@PJL SET AUTOROTATE`, vedi `pjl.rs`): qui c'è solo la previsione usata
//! dall'anteprima, con la stessa regola dei driver HP ("autorotate ruota di
//! 90° se così il foglio entra nel rotolo e consuma meno carta").

/// Tolleranza sulle misure in mm (errori di arrotondamento pt → mm).
pub const TOLLERANZA_MM: f64 = 0.5;

/// Come gestire la rotazione del job.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rotazione {
    /// La stampante ruota quando conviene (`AUTOROTATE=ON`).
    #[default]
    Auto,
    /// Rotazione di 90° richiesta (`AUTOROTATE=ON`, anteprima ruotata).
    Si,
    /// Mai ruotare (`AUTOROTATE=OFF`).
    No,
}

impl Rotazione {
    pub const TUTTE: [Rotazione; 3] = [Rotazione::Auto, Rotazione::Si, Rotazione::No];

    pub fn etichetta(self) -> &'static str {
        match self {
            Rotazione::Auto => "Automatica",
            Rotazione::Si => "Sì, ruota di 90°",
            Rotazione::No => "No",
        }
    }

    /// Valore per `@PJL SET AUTOROTATE`. `Auto` e `Si` chiedono entrambi alla
    /// stampante di ruotare quando conviene; `No` lo vieta. Non esiste un PJL
    /// documentato per forzare i 90° oltre il risparmio carta, quindi `Si`
    /// resta una richiesta: se il foglio ruotato non entra nel rotolo la
    /// stampante stampa senza ruotare.
    pub fn valore_pjl(self) -> &'static str {
        match self {
            Rotazione::Auto | Rotazione::Si => "ON",
            Rotazione::No => "OFF",
        }
    }

    pub fn parse(s: &str) -> Option<Rotazione> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "automatica" => Some(Rotazione::Auto),
            "si" | "sì" | "yes" | "on" => Some(Rotazione::Si),
            "no" | "off" => Some(Rotazione::No),
            _ => None,
        }
    }
}

/// true se ruotare il foglio di 90° conviene: il foglio ruotato entra nel
/// rotolo e (o il foglio dritto non ci entra, o ruotato consuma meno carta).
/// `larghezza` è il lato lungo il rotolo, `altezza` quello che avanza.
pub fn conviene_ruotare(larghezza: f64, altezza: f64, rotolo_mm: f64) -> bool {
    let entra_dritto = larghezza <= rotolo_mm + TOLLERANZA_MM;
    let entra_ruotato = altezza <= rotolo_mm + TOLLERANZA_MM;
    // Ruotato il consumo di carta è `larghezza` invece di `altezza`.
    entra_ruotato && (!entra_dritto || larghezza < altezza)
}

/// true se la pagina va mostrata (e chiesta alla stampante) ruotata di 90°.
/// `Si` chiede la rotazione ma la mostra solo se il foglio ruotato entra nel
/// rotolo: con `@PJL SET AUTOROTATE=ON` la stampante ruota solo in quel caso,
/// quindi una forzatura oltre non sarebbe mantenuta in stampa.
pub fn ruotata(larghezza: f64, altezza: f64, rotolo_mm: f64, modo: Rotazione) -> bool {
    match modo {
        Rotazione::No => false,
        Rotazione::Si => altezza <= rotolo_mm + TOLLERANZA_MM,
        Rotazione::Auto => conviene_ruotare(larghezza, altezza, rotolo_mm),
    }
}

/// Dimensioni effettive di stampa (larghezza lungo il rotolo, consumo di
/// carta) tenuto conto del modo di rotazione.
pub fn dimensioni_stampa(larghezza: f64, altezza: f64, rotolo_mm: f64, modo: Rotazione) -> (f64, f64) {
    if ruotata(larghezza, altezza, rotolo_mm, modo) {
        (altezza, larghezza)
    } else {
        (larghezza, altezza)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn il_foglio_piu_largo_del_rotolo_ruota_se_ci_sta() {
        // L'esempio della issue: A0 orizzontale (1189 × 841) su rotolo da 914.
        assert!(conviene_ruotare(1189.0, 841.0, 914.0));
        // A1 orizzontale (841 × 594) su rotolo da 610.
        assert!(conviene_ruotare(841.0, 594.0, 610.0));
    }

    #[test]
    fn il_verticale_ruota_per_risparmiare_carta() {
        // A1 verticale (594 × 841) su rotolo da 914: dritto consuma 841 mm,
        // ruotato 594 mm ed entra (841 ≤ 914).
        assert!(conviene_ruotare(594.0, 841.0, 914.0));
        // A4 verticale su rotolo da 914: risparmia 87 mm.
        assert!(conviene_ruotare(210.0, 297.0, 914.0));
    }

    #[test]
    fn chi_e_gia_ottimizzato_non_ruota() {
        // A1 orizzontale su rotolo da 914: già col lato corto avanti.
        assert!(!conviene_ruotare(841.0, 594.0, 914.0));
        // Foglio quadrato: ruotare non cambia niente.
        assert!(!conviene_ruotare(500.0, 500.0, 914.0));
    }

    #[test]
    fn se_non_ci_sta_in_nessun_verso_non_ruota() {
        // Niente da guadagnare: la stampa verrà comunque tagliata o scalata.
        assert!(!conviene_ruotare(1200.0, 1000.0, 914.0));
        // Verticale A0 (841 × 1189) su rotolo da 914: ruotato non ci entra.
        assert!(!conviene_ruotare(841.0, 1189.0, 914.0));
    }

    #[test]
    fn modi_forzati() {
        // `Si` chiede la rotazione: A1 orizzontale su rotolo da 914 ruotato ci
        // sta e l'anteprima lo mostra (la stampante poi ruota solo se risparmia).
        assert!(ruotata(841.0, 594.0, 914.0, Rotazione::Si));
        assert!(!ruotata(594.0, 841.0, 914.0, Rotazione::No));
        // `Si` su un foglio che ruotato non entra (594 × 841 su rotolo da 610):
        // la stampante non ruoterebbe, l'anteprima resta dritta.
        assert!(!ruotata(594.0, 841.0, 610.0, Rotazione::Si));
        assert_eq!(dimensioni_stampa(594.0, 841.0, 610.0, Rotazione::Si), (594.0, 841.0));
        assert_eq!(dimensioni_stampa(594.0, 841.0, 914.0, Rotazione::Auto), (841.0, 594.0));
        assert_eq!(dimensioni_stampa(594.0, 841.0, 914.0, Rotazione::No), (594.0, 841.0));
        assert_eq!(Rotazione::Auto.valore_pjl(), "ON");
        assert_eq!(Rotazione::Si.valore_pjl(), "ON");
        assert_eq!(Rotazione::No.valore_pjl(), "OFF");
    }

    #[test]
    fn parse_da_riga_di_comando_e_config() {
        assert_eq!(Rotazione::parse("auto"), Some(Rotazione::Auto));
        assert_eq!(Rotazione::parse("SI"), Some(Rotazione::Si));
        assert_eq!(Rotazione::parse("no"), Some(Rotazione::No));
        assert_eq!(Rotazione::parse("boh"), None);
    }
}

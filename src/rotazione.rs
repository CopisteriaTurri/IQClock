//! Previsione della rotazione automatica di 90° fatta dalla stampante.
//!
//! La T2600 ruota da sola i job quando così il foglio consuma meno carta
//! (impostazione del pannello). Il PJL non la controlla: provato il
//! 29/10/2026, `@PJL SET AUTOROTATE=OFF` viene ignorato e `ON` non cambia
//! niente. Qui c'è solo la regola: l'anteprima la usa per mostrare come
//! uscirà il foglio, e `impagina.rs` per ruotare da sé le pagine che allarga al
//! rotolo (quelle la stampante non le ruota più).
//!
//! Verificato con un A5 verticale (esce ruotato) e un A5 orizzontale (esce
//! dritto) su rotolo da 36"; il caso "più largo del rotolo" non è provato.

/// Tolleranza sulle misure in mm (errori di arrotondamento pt → mm).
pub const TOLLERANZA_MM: f64 = 0.5;

/// true se la stampante ruota il foglio di 90°: ruotato entra nel rotolo e
/// (o dritto non ci entra, o ruotato consuma meno carta).
/// `larghezza` è il lato lungo il rotolo, `altezza` quello che avanza.
pub fn ruota(larghezza: f64, altezza: f64, rotolo_mm: f64) -> bool {
    let entra_dritto = larghezza <= rotolo_mm + TOLLERANZA_MM;
    let entra_ruotato = altezza <= rotolo_mm + TOLLERANZA_MM;
    // Ruotato il consumo di carta è `larghezza` invece di `altezza`.
    entra_ruotato && (!entra_dritto || larghezza < altezza)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn verticale_ruota_per_risparmiare_carta() {
        // Provato sulla T2600: A5 verticale su rotolo da 914 esce ruotato.
        assert!(ruota(148.0, 210.0, 914.0));
        // A1 verticale (594 × 841): ruotato consuma 594 mm ed entra.
        assert!(ruota(594.0, 841.0, 914.0));
    }

    #[test]
    fn orizzontale_resta_dritto() {
        // Provato sulla T2600: A5 orizzontale su rotolo da 914 esce dritto.
        assert!(!ruota(210.0, 148.0, 914.0));
        assert!(!ruota(841.0, 594.0, 914.0));
        // Foglio quadrato: ruotare non cambia niente.
        assert!(!ruota(500.0, 500.0, 914.0));
    }

    #[test]
    fn piu_largo_del_rotolo_ruota_se_ci_sta() {
        // L'esempio della issue: A0 orizzontale (1189 × 841) su rotolo da 914.
        assert!(ruota(1189.0, 841.0, 914.0));
        // A1 orizzontale (841 × 594) su rotolo da 610.
        assert!(ruota(841.0, 594.0, 610.0));
    }

    #[test]
    fn se_non_ci_sta_in_nessun_verso_non_ruota() {
        assert!(!ruota(1200.0, 1000.0, 914.0));
        // A0 verticale (841 × 1189) su rotolo da 914: ruotato non ci entra.
        assert!(!ruota(841.0, 1189.0, 914.0));
        // Verticale che ruotato non entra (594 × 841 su rotolo da 610).
        assert!(!ruota(594.0, 841.0, 610.0));
    }
}

//! Foglio singolo al posto del rotolo (#14).
//!
//! Con `Sorgente::Foglio` la carta la prende l'ingresso manuale della
//! stampante: qui c'è il formato del foglio, per disegnare l'anteprima al posto
//! del rotolo, dire se la pagina ci entra e calcolare l'area utile per
//! l'adattamento opzionale. Non è
//! impaginazione: niente larghezza del rotolo, niente allineamento, niente
//! nesting.
//!
//! Le misure le può dichiarare la stampante: sulla T2600, con un A4 caricato
//! nell'ingresso manuale, la riga HP del `MANUAL_FEED` riporta 5952 × 8418
//! decipoint = 210 × 297 mm (letto il 06/11/2026). Se non le dichiara resta il
//! formato scelto a mano.
//!
//! Il foglio deve essere **più grande** della pagina: sulla T2600 un A4 (210 ×
//! 297 mm) chiede un foglio di 220 × 319 mm (letto dal pannello il 06/11/2026),
//! cioè 5 mm per lato e 22 mm sul lato d'entrata, che è il margine che HP
//! documenta per la stampa su foglio. Per questo `entra` non si limita a
//! confrontare i lati: un A4 su un foglio A4 non basta.
//!
//! **Non verificato sulla T2600** (06/11/2026): se la stampante ruota da sola
//! una pagina che ci starebbe solo girata, e i 5 mm ai lati su formati diversi
//! dall'A4 (li ho visti solo su un A4).

use crate::lavorazione::FormatoIso;
use crate::rotazione::TOLLERANZA_MM;

/// Spazio che la stampante si prende ai lati del foglio.
pub const MARGINE_LATO_MM: f64 = 5.0;

/// Spazio che la stampante si prende sul lato d'entrata del foglio: 22 mm, il
/// margine che HP dichiara per la stampa su foglio della T2600 (3 × 22 × 3 × 3
/// mm). Un A4 chiede così 319 mm di lunghezza.
pub const MARGINE_ENTRATA_MM: f64 = 22.0;

/// Formati ISO scelti dal menù, dal più piccolo al più grande.
pub const FORMATI: [FormatoIso; 5] = [
    FormatoIso::A4,
    FormatoIso::A3,
    FormatoIso::A2,
    FormatoIso::A1,
    FormatoIso::A0,
];

/// Da dove vengono le misure del foglio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SceltaFoglio {
    /// Quelle che la stampante dichiara per l'ingresso manuale.
    Stampante,
    /// Un formato ISO, verticale o orizzontale.
    Iso(FormatoIso),
    /// Misura scelta a mano.
    Mano,
}

/// Formato del foglio su cui esce il job: le misure sono quelle del foglio
/// caricato, non quelle del file (la stampante non le legge da qui).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Foglio {
    pub scelta: SceltaFoglio,
    pub larghezza_mm: f64,
    pub altezza_mm: f64,
}

impl Default for Foglio {
    /// A4 verticale finché la stampante non dice altro: è il foglio singolo
    /// più comune.
    fn default() -> Self {
        Self::iso(FormatoIso::A4, false)
    }
}

impl Foglio {
    /// Area stampabile in lati (corto, lungo), esclusi i margini della stampante.
    pub fn area_utile(&self) -> (f64, f64) {
        (
            self.larghezza_mm.min(self.altezza_mm) - 2.0 * MARGINE_LATO_MM,
            self.larghezza_mm.max(self.altezza_mm) - MARGINE_ENTRATA_MM,
        )
    }

    /// Foglio del formato ISO `formato`, verticale o orizzontale.
    pub fn iso(formato: FormatoIso, orizzontale: bool) -> Self {
        let (corto, lungo) = formato.mm();
        let (larghezza_mm, altezza_mm) =
            if orizzontale { (lungo, corto) } else { (corto, lungo) };
        Self { scelta: SceltaFoglio::Iso(formato), larghezza_mm, altezza_mm }
    }

    /// Prende le misure dichiarate dalla stampante, se la scelta è ancora
    /// quella automatica: un formato ISO o una misura a mano non si toccano.
    pub fn aggiorna_da_stampante(&mut self, letto: Option<(f64, f64)>) {
        if self.scelta == SceltaFoglio::Stampante
            && let Some((l, a)) = letto
        {
            self.larghezza_mm = l;
            self.altezza_mm = a;
        }
    }

    /// true se il lato lungo sta in altezza.
    pub fn verticale(&self) -> bool {
        self.altezza_mm >= self.larghezza_mm
    }

    /// Etichetta per il menù e l'anteprima: "A4 verticale", "300 × 200 mm".
    pub fn etichetta(&self) -> String {
        match self.scelta {
            SceltaFoglio::Stampante => {
                format!("Dalla stampante: {:.0} × {:.0} mm", self.larghezza_mm, self.altezza_mm)
            }
            SceltaFoglio::Iso(f) => format!(
                "{} {}",
                f.etichetta(),
                if self.verticale() { "verticale" } else { "orizzontale" }
            ),
            SceltaFoglio::Mano => {
                format!("{:.0} × {:.0} mm", self.larghezza_mm, self.altezza_mm)
            }
        }
    }

    /// Gira il foglio di 90° (un formato ISO resta quello scelto).
    pub fn ruota(&mut self) {
        std::mem::swap(&mut self.larghezza_mm, &mut self.altezza_mm);
    }

    /// Foglio minimo per una pagina di `pagina` mm, in lati (corto, lungo):
    /// così si confronta con le misure lette dalla stampante, che non dicono
    /// in che verso è il foglio. Per un A4 sono (220, 319).
    pub fn minimo_lati(pagina: (f64, f64)) -> (f64, f64) {
        let (corto, lungo) = (pagina.0.min(pagina.1), pagina.0.max(pagina.1));
        (
            corto + 2.0 * MARGINE_LATO_MM,
            lungo + MARGINE_ENTRATA_MM,
        )
    }

    /// Foglio più piccolo che la stampante accetta per una pagina di `pagina`
    /// mm, con i suoi margini: per un A4 sono 220 × 319 mm. Le misure tornano
    /// nello stesso verso della pagina.
    pub fn foglio_minimo(pagina: (f64, f64)) -> (f64, f64) {
        let (lar, alt) = Self::minimo_lati(pagina);
        if pagina.1 >= pagina.0 { (lar, alt) } else { (alt, lar) }
    }

    /// true se una pagina di `pagina` mm ci entra nel foglio, margini della
    /// stampante compresi. I lati si confrontano in ordine, quindi una pagina
    /// orizzontale entra in un foglio verticale se la stampante la gira: la
    /// rotazione sul foglio non è stata provata. Misure ignote (0) non fanno
    /// avvisare.
    pub fn entra(&self, pagina: (f64, f64)) -> bool {
        let (l, a) = pagina;
        if !(l > 0.0 && a > 0.0 && self.larghezza_mm > 0.0 && self.altezza_mm > 0.0) {
            return true;
        }
        let (corto, lungo) = (l.min(a), l.max(a));
        let (f_corto, f_lungo) = (
            self.larghezza_mm.min(self.altezza_mm),
            self.larghezza_mm.max(self.altezza_mm),
        );
        corto + 2.0 * MARGINE_LATO_MM <= f_corto + TOLLERANZA_MM
            && lungo + MARGINE_ENTRATA_MM <= f_lungo + TOLLERANZA_MM
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a4_di_default_e_verticale() {
        let f = Foglio::default();
        assert_eq!((f.larghezza_mm, f.altezza_mm), (210.0, 297.0));
        assert_eq!(f.scelta, SceltaFoglio::Iso(FormatoIso::A4));
        assert!(f.verticale());
        assert_eq!(f.etichetta(), "A4 verticale");
    }

    #[test]
    fn orizzontale_scambia_i_lati() {
        let f = Foglio::iso(FormatoIso::A3, true);
        assert_eq!((f.larghezza_mm, f.altezza_mm), (420.0, 297.0));
        assert!(!f.verticale());
        assert_eq!(f.etichetta(), "A3 orizzontale");
        let mut v = Foglio::iso(FormatoIso::A3, false);
        v.ruota();
        assert_eq!((v.larghezza_mm, v.altezza_mm), (420.0, 297.0));
        assert_eq!(v.scelta, SceltaFoglio::Iso(FormatoIso::A3), "girare non perde il formato");
    }

    /// Il foglio minimo è quello che la stampante chiede davvero: per un A4
    /// 220 × 319 mm, misurato sulla T2600 il 06/11/2026.
    #[test]
    fn il_foglio_minimo_porta_i_margini_della_stampante() {
        assert_eq!(Foglio::foglio_minimo((210.0, 297.0)), (220.0, 319.0));
        // Una pagina sdraiata si misura allo stesso modo, nel suo verso.
        assert_eq!(Foglio::foglio_minimo((297.0, 210.0)), (319.0, 220.0));
        // In lati (corto, lungo) la misura non dipende dal verso: serve a
        // confrontarla con il foglio caricato, che la stampante dà largo × alto.
        assert_eq!(Foglio::minimo_lati((210.0, 297.0)), (220.0, 319.0));
        assert_eq!(Foglio::minimo_lati((297.0, 210.0)), (220.0, 319.0));
    }

    #[test]
    fn la_pagina_entra_o_non_entra() {
        // Un A4 su un foglio A4 non basta: prima di stampare la T2600 chiede
        // 220 × 319 mm, e infatti con l'A4 caricato ha chiesto un foglio più
        // grande invece di stampare.
        let a4 = Foglio::default();
        assert!(!a4.entra((210.0, 297.0)));
        let giusto =
            Foglio { scelta: SceltaFoglio::Mano, larghezza_mm: 220.0, altezza_mm: 319.0 };
        assert!(giusto.entra((210.0, 297.0)));
        // Orientata diversamente: ci sta girata (la rotazione non è provata).
        assert!(giusto.entra((297.0, 210.0)));
        // Arrotondamenti dei punti del PDF: un A4 da 595 × 842 pt è 210 × 297 mm.
        assert!(giusto.entra((210.0014, 297.0014)));
        assert!(giusto.entra((210.0, 297.0 + TOLLERANZA_MM)));
        assert!(!giusto.entra((210.0, 297.0 + TOLLERANZA_MM + 0.1)));
        assert!(!giusto.entra((420.0, 297.0)), "un A3 non entra in un foglio da A4 + margini");
        // Misure ignote: non si avvisa.
        assert!(giusto.entra((0.0, 0.0)));
    }

    #[test]
    fn misura_a_mano() {
        let f = Foglio { scelta: SceltaFoglio::Mano, larghezza_mm: 300.0, altezza_mm: 200.0 };
        assert_eq!(f.etichetta(), "300 × 200 mm");
        assert!(!f.verticale());
        // 270 × 190 più 5 mm per lato e 22 sul lato d'entrata: 280 × 212, e i
        // lati si confrontano in ordine con i 200 × 300 del foglio.
        assert!(f.entra((270.0, 190.0)));
        assert!(!f.entra((210.0, 297.0)), "un A4 vuole 220 × 319 mm");
    }

    #[test]
    fn la_stampante_aggiorna_solo_la_scelta_automatica() {
        let mut f = Foglio { scelta: SceltaFoglio::Stampante, ..Default::default() };
        f.aggiorna_da_stampante(Some((210.0, 297.0)));
        assert_eq!((f.larghezza_mm, f.altezza_mm), (210.0, 297.0));
        assert_eq!(f.etichetta(), "Dalla stampante: 210 × 297 mm");
        // Una lettura assente lascia le ultime misure note.
        f.aggiorna_da_stampante(None);
        assert_eq!(f.larghezza_mm, 210.0);
        // Un formato scelto a mano non si fa sovrascrivere.
        let mut a3 = Foglio::iso(FormatoIso::A3, false);
        a3.aggiorna_da_stampante(Some((210.0, 297.0)));
        assert_eq!((a3.larghezza_mm, a3.altezza_mm), (297.0, 420.0));
    }
}

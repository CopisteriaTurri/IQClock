//! Costruzione delle intestazioni PJL (Printer Job Language) per i DesignJet.
//!
//! Riferimenti:
//! - HP "Printer Job Language Technical Reference Manual" (2003): UEL, JOB/EOJ,
//!   ENTER LANGUAGE, COPIES, RENDERMODE. Righe terminate da CR LF.
//!   (`INFO STATUS` sulla T2600 risponde solo `"?"`: lo stato si legge via SNMP.)
//! - MEDIASOURCE=ROLL1..ROLL6/AUTO: tabella PJL dei DesignJet recenti (XL 3600).
//! - CUTTER=ON/OFF: visto in driver CUPS per DesignJet, **non documentato** nel
//!   manuale: per questo è disattivato di default.
//! - Allineamento sul rotolo (sinistra/centro/destra): nessuna variabile PJL
//!   verificata sulla T2600, quindi `Allineamento` vive solo nella GUI e
//!   nell'anteprima e `intestazione` non lo manda.

use std::fmt::Write as _;

/// Universal Exit Language: deve aprire e chiudere ogni job PJL.
pub const UEL: &[u8] = b"\x1b%-12345X";

/// Formato del file, ricavato dai primi byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Formato {
    Pdf,
    PostScript,
    Tiff,
    Jpeg,
    /// Convertito in TIFF prima dell'invio (vedi `png.rs`).
    Png,
    Hpgl2,
}

impl Formato {
    /// Nome per `@PJL ENTER LANGUAGE`. TIFF e JPEG non hanno un nome documentato:
    /// per quelli non si manda ENTER e la stampante riconosce il formato da sola.
    /// I PNG viaggiano come TIFF, quindi nemmeno loro hanno ENTER LANGUAGE.
    pub fn linguaggio_pjl(self) -> Option<&'static str> {
        match self {
            Formato::Pdf => Some("PDF"),
            Formato::PostScript => Some("POSTSCRIPT"),
            Formato::Hpgl2 => Some("HPGL2"),
            Formato::Tiff | Formato::Jpeg | Formato::Png => None,
        }
    }

    pub fn etichetta(self) -> &'static str {
        match self {
            Formato::Pdf => "PDF",
            Formato::PostScript => "PostScript",
            Formato::Tiff => "TIFF",
            Formato::Jpeg => "JPEG",
            Formato::Png => "PNG",
            Formato::Hpgl2 => "HP-GL/2",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Allineamento {
    /// Il contenuto sta a sinistra del rotolo.
    #[default]
    Sinistra,
    Centrato,
    Destra,
}

impl Allineamento {
    pub const TUTTI: [Allineamento; 3] = [Allineamento::Sinistra, Allineamento::Centrato, Allineamento::Destra];

    pub fn etichetta(self) -> &'static str {
        match self {
            Allineamento::Sinistra => "Sinistra",
            Allineamento::Centrato => "Centro",
            Allineamento::Destra => "Destra",
        }
    }

    /// Ascissa del bordo sinistro di un contenuto largo `larghezza_contenuto`
    /// dentro un rotolo che inizia a `bordo_sinistro` e vale `larghezza_rotolo`.
    /// Se il contenuto è più largo del rotolo resta a sinistra.
    /// (Quando si farà il nesting della issue #8, l'allineamento del blocco
    /// userà questo stesso enum.)
    pub fn x(self, bordo_sinistro: f32, larghezza_rotolo: f32, larghezza_contenuto: f32) -> f32 {
        let scarto = (larghezza_rotolo - larghezza_contenuto).max(0.0);
        match self {
            Allineamento::Sinistra => bordo_sinistro,
            Allineamento::Centrato => bordo_sinistro + scarto / 2.0,
            Allineamento::Destra => bordo_sinistro + scarto,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sorgente {
    /// Non manda MEDIASOURCE: decide la stampante (impostazione del pannello).
    Stampante,
    Auto,
    Rotolo1,
    Rotolo2,
}

impl Sorgente {
    pub const TUTTE: [Sorgente; 4] = [
        Sorgente::Stampante,
        Sorgente::Auto,
        Sorgente::Rotolo1,
        Sorgente::Rotolo2,
    ];

    fn valore_pjl(self) -> Option<&'static str> {
        match self {
            Sorgente::Stampante => None,
            Sorgente::Auto => Some("AUTO"),
            Sorgente::Rotolo1 => Some("ROLL1"),
            Sorgente::Rotolo2 => Some("ROLL2"),
        }
    }

    pub fn etichetta(self) -> &'static str {
        match self {
            Sorgente::Stampante => "Come da stampante",
            Sorgente::Auto => "Automatico",
            Sorgente::Rotolo1 => "Rotolo 1",
            Sorgente::Rotolo2 => "Rotolo 2",
        }
    }
}

#[derive(Clone, Debug)]
pub struct OpzioniJob {
    pub nome: String,
    pub copie: u32,
    pub sorgente: Sorgente,
    pub scala_di_grigi: bool,
    /// `None` = non mandare CUTTER (comando non documentato).
    pub taglierina: Option<bool>,
    /// Righe `@PJL ...` aggiuntive, una per riga, mandate così come sono.
    pub extra: String,
}

impl Default for OpzioniJob {
    fn default() -> Self {
        Self {
            nome: String::new(),
            copie: 1,
            sorgente: Sorgente::Stampante,
            scala_di_grigi: false,
            taglierina: None,
            extra: String::new(),
        }
    }
}

/// Nome job sicuro: niente virgolette né caratteri di controllo, max 80 caratteri.
fn pulisci_nome(nome: &str) -> String {
    nome.chars()
        .filter(|c| !c.is_control() && *c != '"')
        .take(80)
        .collect()
}

/// Testo PJL che precede i dati del file (UEL incluso).
pub fn intestazione(opz: &OpzioniJob, formato: Formato) -> Vec<u8> {
    let mut s = String::new();
    let nome = pulisci_nome(&opz.nome);
    let _ = write!(s, "@PJL JOB NAME=\"{nome}\"\r\n");
    let _ = write!(s, "@PJL SET COPIES={}\r\n", opz.copie.max(1));
    if let Some(v) = opz.sorgente.valore_pjl() {
        let _ = write!(s, "@PJL SET MEDIASOURCE={v}\r\n");
    }
    let resa = if opz.scala_di_grigi { "GRAYSCALE" } else { "COLOR" };
    let _ = write!(s, "@PJL SET RENDERMODE={resa}\r\n");
    if let Some(t) = opz.taglierina {
        let _ = write!(s, "@PJL SET CUTTER={}\r\n", if t { "ON" } else { "OFF" });
    }
    for riga in opz.extra.lines().map(str::trim).filter(|r| r.starts_with("@PJL")) {
        let _ = write!(s, "{riga}\r\n");
    }
    if let Some(lingua) = formato.linguaggio_pjl() {
        let _ = write!(s, "@PJL ENTER LANGUAGE={lingua}\r\n");
    }

    let mut out = UEL.to_vec();
    out.extend_from_slice(s.as_bytes());
    out
}

/// Testo PJL che segue i dati del file.
pub fn chiusura(opz: &OpzioniJob) -> Vec<u8> {
    let mut out = UEL.to_vec();
    out.extend_from_slice(format!("@PJL EOJ NAME=\"{}\"\r\n", pulisci_nome(&opz.nome)).as_bytes());
    out.extend_from_slice(UEL);
    out
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn intestazione_pdf_minima() {
        let opz = OpzioniJob { nome: "tavola \"A1\"".into(), ..Default::default() };
        let h = intestazione(&opz, Formato::Pdf);
        assert_eq!(
            String::from_utf8(h).unwrap(),
            "\x1b%-12345X@PJL JOB NAME=\"tavola A1\"\r\n\
             @PJL SET COPIES=1\r\n\
             @PJL SET RENDERMODE=COLOR\r\n\
             @PJL ENTER LANGUAGE=PDF\r\n"
        );
    }

    #[test]
    fn intestazione_completa() {
        let opz = OpzioniJob {
            nome: "x".into(),
            copie: 3,
            sorgente: Sorgente::Rotolo2,
            scala_di_grigi: true,
            taglierina: Some(false),
            extra: "@PJL SET MIRROR=OFF\nroba non pjl\n".into(),
        };
        let h = String::from_utf8(intestazione(&opz, Formato::Jpeg)).unwrap();
        assert!(h.contains("@PJL SET COPIES=3\r\n"));
        assert!(h.contains("@PJL SET MEDIASOURCE=ROLL2\r\n"));
        assert!(h.contains("@PJL SET RENDERMODE=GRAYSCALE\r\n"));
        assert!(h.contains("@PJL SET CUTTER=OFF\r\n"));
        assert!(h.contains("@PJL SET MIRROR=OFF\r\n"));
        assert!(!h.contains("roba"));
        assert!(!h.contains("ENTER LANGUAGE"));
    }

    #[test]
    fn chiusura_termina_con_uel() {
        let c = chiusura(&OpzioniJob::default());
        assert!(c.starts_with(UEL) && c.ends_with(UEL));
    }

    #[test]
    fn allineamento_default_sinistra() {
        assert_eq!(Allineamento::default(), Allineamento::Sinistra);
        assert_eq!(Allineamento::Sinistra.etichetta(), "Sinistra");
    }

    #[test]
    fn allineamento_sposta_solo_se_ci_sta() {
        // Rotolo da x=10 largo 100, contenuto largo 40.
        assert_eq!(Allineamento::Sinistra.x(10.0, 100.0, 40.0), 10.0);
        assert_eq!(Allineamento::Centrato.x(10.0, 100.0, 40.0), 40.0);
        assert_eq!(Allineamento::Destra.x(10.0, 100.0, 40.0), 70.0);
        // Contenuto più largo del rotolo: resta a sinistra.
        assert_eq!(Allineamento::Centrato.x(10.0, 100.0, 120.0), 10.0);
        assert_eq!(Allineamento::Destra.x(10.0, 100.0, 120.0), 10.0);
    }
}

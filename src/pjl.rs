//! Costruzione delle intestazioni PJL (Printer Job Language) per i DesignJet.
//!
//! Riferimenti:
//! - HP "Printer Job Language Technical Reference Manual" (2003): UEL, JOB/EOJ,
//!   ENTER LANGUAGE, COPIES, RENDERMODE. Righe terminate da CR LF.
//!   (`INFO STATUS` sulla T2600 risponde solo `"?"`: lo stato si legge via SNMP.)
//! - MEDIASOURCE=ROLL1..ROLL6/AUTO: tabella PJL dei DesignJet recenti (XL 3600).
//! - CUTTER=ON/OFF: visto in driver CUPS per DesignJet, **non documentato** nel
//!   manuale: per questo è disattivato di default.

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
}

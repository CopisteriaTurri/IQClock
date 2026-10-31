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

use crate::impagina::Impaginazione;
use crate::lavorazione::Lavorazione;

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
    /// Allineamento sul rotolo. Non finisce nel PJL: cambia il file che parte
    /// (vedi `impagina.rs`); `None` = il file parte com'è.
    pub impaginazione: Option<Impaginazione>,
    /// Scala del documento, applicata prima dell'impaginazione (vedi
    /// `lavorazione.rs`).
    pub lavorazione: Lavorazione,
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
            impaginazione: None,
            lavorazione: Lavorazione::default(),
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

    const FORMATI: [Formato; 6] = [
        Formato::Pdf,
        Formato::PostScript,
        Formato::Tiff,
        Formato::Jpeg,
        Formato::Png,
        Formato::Hpgl2,
    ];

    /// Intestazione attesa scritta seguendo il manuale PJL senza riusare il
    /// codice sotto test: apertura con UEL e ogni riga chiusa da CR LF.
    fn attesa_intestazione(opz: &OpzioniJob, formato: Formato) -> String {
        let mut s = String::from("\x1b%-12345X");
        s += &format!("@PJL JOB NAME=\"{}\"\r\n", opz.nome);
        s += &format!("@PJL SET COPIES={}\r\n", opz.copie.max(1));
        match opz.sorgente {
            Sorgente::Stampante => {}
            Sorgente::Auto => s += "@PJL SET MEDIASOURCE=AUTO\r\n",
            Sorgente::Rotolo1 => s += "@PJL SET MEDIASOURCE=ROLL1\r\n",
            Sorgente::Rotolo2 => s += "@PJL SET MEDIASOURCE=ROLL2\r\n",
        }
        s += if opz.scala_di_grigi {
            "@PJL SET RENDERMODE=GRAYSCALE\r\n"
        } else {
            "@PJL SET RENDERMODE=COLOR\r\n"
        };
        match opz.taglierina {
            None => {}
            Some(true) => s += "@PJL SET CUTTER=ON\r\n",
            Some(false) => s += "@PJL SET CUTTER=OFF\r\n",
        }
        for riga in opz.extra.lines().map(str::trim).filter(|r| r.starts_with("@PJL")) {
            s += &format!("{riga}\r\n");
        }
        match formato {
            Formato::Pdf => s += "@PJL ENTER LANGUAGE=PDF\r\n",
            Formato::PostScript => s += "@PJL ENTER LANGUAGE=POSTSCRIPT\r\n",
            Formato::Hpgl2 => s += "@PJL ENTER LANGUAGE=HPGL2\r\n",
            Formato::Tiff | Formato::Jpeg | Formato::Png => {}
        }
        s
    }

    fn attesa_chiusura(nome: &str) -> String {
        format!("\x1b%-12345X@PJL EOJ NAME=\"{nome}\"\r\n\x1b%-12345X")
    }

    fn testo(v: Vec<u8>) -> String {
        String::from_utf8(v).expect("PJL deve essere ASCII/UTF-8")
    }

    #[test]
    fn uel_esatto() {
        assert_eq!(UEL, b"\x1b%-12345X".as_slice());
    }

    #[test]
    fn opzioni_predefinite() {
        let opz = OpzioniJob::default();
        assert_eq!(opz.nome, "");
        assert_eq!(opz.copie, 1);
        assert_eq!(opz.sorgente, Sorgente::Stampante);
        assert!(!opz.scala_di_grigi);
        assert_eq!(opz.taglierina, None);
        assert!(opz.extra.is_empty());
    }

    #[test]
    fn intestazione_pdf_minima() {
        let opz = OpzioniJob { nome: "tavola \"A1\"".into(), ..Default::default() };
        assert_eq!(
            testo(intestazione(&opz, Formato::Pdf)),
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
            impaginazione: None,
            lavorazione: Lavorazione::default(),
        };
        assert_eq!(
            testo(intestazione(&opz, Formato::Jpeg)),
            "\x1b%-12345X@PJL JOB NAME=\"x\"\r\n\
             @PJL SET COPIES=3\r\n\
             @PJL SET MEDIASOURCE=ROLL2\r\n\
             @PJL SET RENDERMODE=GRAYSCALE\r\n\
             @PJL SET CUTTER=OFF\r\n\
             @PJL SET MIRROR=OFF\r\n"
        );
    }

    /// Tutte le combinazioni di formato, sorgente, resa, taglierina, copie ed
    /// extra: l'output deve combaciare byte per byte con l'atteso.
    #[test]
    fn tutte_le_combinazioni() {
        let extra_da_provare = [
            "",
            "@PJL SET MIRROR=OFF\n",
            "  @PJL SET A=1  \r\nroba\n\n@PJL SET B=\"2\"",
        ];
        for formato in FORMATI {
            for sorgente in Sorgente::TUTTE {
                for scala_di_grigi in [false, true] {
                    for taglierina in [None, Some(true), Some(false)] {
                        for copie in [0, 1, 2, 7] {
                            for extra in extra_da_provare {
                                let opz = OpzioniJob {
                                    nome: "prova.pdf".into(),
                                    copie,
                                    sorgente,
                                    scala_di_grigi,
                                    taglierina,
                                    extra: extra.into(),
                                    impaginazione: None,
                                    lavorazione: Lavorazione::default(),
                                };
                                let atteso = attesa_intestazione(&opz, formato);
                                let got = testo(intestazione(&opz, formato));
                                assert_eq!(
                                    got, atteso,
                                    "{formato:?} {sorgente:?} grigi={scala_di_grigi} \
                                     cutter={taglierina:?} copie={copie} extra={extra:?}"
                                );
                                assert!(!got.ends_with('\r'), "CR finale: {got:?}");
                                for (i, b) in got.bytes().enumerate() {
                                    if b == b'\n' {
                                        assert!(
                                            i > 0 && got.as_bytes()[i - 1] == b'\r',
                                            "riga senza CR: {got:?} ({formato:?})"
                                        );
                                    }
                                }
                                for coppia in got.as_bytes().windows(2) {
                                    assert!(
                                        !(coppia[0] == b'\r' && coppia[1] != b'\n'),
                                        "CR non seguito da LF: {got:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn copie_zero_diventa_uno() {
        let opz = OpzioniJob { copie: 0, ..Default::default() };
        assert!(testo(intestazione(&opz, Formato::Pdf)).contains("@PJL SET COPIES=1\r\n"));
    }

    #[test]
    fn linguaggio_per_formato() {
        assert_eq!(Formato::Pdf.linguaggio_pjl(), Some("PDF"));
        assert_eq!(Formato::PostScript.linguaggio_pjl(), Some("POSTSCRIPT"));
        assert_eq!(Formato::Hpgl2.linguaggio_pjl(), Some("HPGL2"));
        assert_eq!(Formato::Tiff.linguaggio_pjl(), None);
        assert_eq!(Formato::Jpeg.linguaggio_pjl(), None);
        assert_eq!(Formato::Png.linguaggio_pjl(), None);
    }

    #[test]
    fn enter_language_solo_dove_documentato() {
        for formato in FORMATI {
            let h = testo(intestazione(&OpzioniJob::default(), formato));
            let atteso = match formato.linguaggio_pjl() {
                Some(l) => format!("@PJL ENTER LANGUAGE={l}\r\n"),
                None => String::new(),
            };
            assert!(h.ends_with(&atteso), "{formato:?}");
            assert_eq!(h.matches("ENTER LANGUAGE").count(), usize::from(!atteso.is_empty()));
        }
    }

    #[test]
    fn nome_job_pulito() {
        let opz = OpzioniJob {
            nome: "fat\"tura\r\n2026\t\0.pdf".into(),
            ..Default::default()
        };
        assert_eq!(
            testo(intestazione(&opz, Formato::Tiff)),
            "\x1b%-12345X@PJL JOB NAME=\"fattura2026.pdf\"\r\n\
             @PJL SET COPIES=1\r\n\
             @PJL SET RENDERMODE=COLOR\r\n"
        );
        assert_eq!(testo(chiusura(&opz)), attesa_chiusura("fattura2026.pdf"));
    }

    #[test]
    fn nome_job_troncato_a_80_caratteri() {
        let h = testo(intestazione(
            &OpzioniJob { nome: "x".repeat(120), ..Default::default() },
            Formato::Pdf,
        ));
        assert!(h.contains(&format!("@PJL JOB NAME=\"{}\"\r\n", "x".repeat(80))), "{h:?}");
        assert!(!h.contains("x".repeat(81).as_str()));
    }

    #[test]
    fn nome_job_multibyte_troncato_per_caratteri() {
        let h = testo(intestazione(
            &OpzioniJob { nome: "\u{e0}".repeat(100), ..Default::default() },
            Formato::Pdf,
        ));
        assert!(h.contains(&format!("@PJL JOB NAME=\"{}\"\r\n", "\u{e0}".repeat(80))), "{h:?}");
    }

    #[test]
    fn chiusura_byte_per_byte() {
        let opz = OpzioniJob { nome: "disegno \"1\".pdf".into(), ..Default::default() };
        assert_eq!(
            testo(chiusura(&opz)),
            "\x1b%-12345X@PJL EOJ NAME=\"disegno 1.pdf\"\r\n\x1b%-12345X"
        );
    }

    #[test]
    fn chiusura_ignora_le_altre_opzioni() {
        let opz = OpzioniJob {
            nome: "x.pdf".into(),
            copie: 9,
            sorgente: Sorgente::Rotolo1,
            scala_di_grigi: true,
            taglierina: Some(true),
            extra: "@PJL SET FOO=1".into(),
            impaginazione: None,
            lavorazione: Lavorazione::default(),
        };
        assert_eq!(testo(chiusura(&opz)), attesa_chiusura("x.pdf"));
        let solo_nome = OpzioniJob { nome: "x.pdf".into(), ..Default::default() };
        assert_eq!(chiusura(&opz), chiusura(&solo_nome));
    }

    #[test]
    fn chiusura_termina_con_uel() {
        let c = chiusura(&OpzioniJob::default());
        assert!(c.starts_with(UEL) && c.ends_with(UEL));
        assert_eq!(testo(c), attesa_chiusura(""));
    }
}

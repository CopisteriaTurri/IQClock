//! Stato della stampante via SNMP: modello, stato, inchiostri, ingressi carta.
//! OID verificati su una DesignJet T2600 PS MFP (firmware JetDirect JD151).

use anyhow::Result;

use crate::snmp::{self, Client, Valore};

const MODELLO: &str = "1.3.6.1.2.1.25.3.2.1.3.1"; // hrDeviceDescr
const STATO_STAMPANTE: &str = "1.3.6.1.2.1.25.3.5.1.1.1"; // hrPrinterStatus
const STATO_DISPOSITIVO: &str = "1.3.6.1.2.1.25.3.2.1.5.1"; // hrDeviceStatus
const NOMI_CARTUCCE: &str = "1.3.6.1.2.1.43.11.1.1.6.1"; // prtMarkerSuppliesDescription
const NOMI_INGRESSI: &str = "1.3.6.1.2.1.43.8.2.1.13.1"; // prtInputName
const STATI_INGRESSI: &str = "1.3.6.1.2.1.43.8.2.1.11.1"; // prtInputStatus
const MEDIA_INGRESSI: &str = "1.3.6.1.2.1.43.8.2.1.12.1"; // prtInputMediaName
/// MIB privata HP (passerella PML): tabella della carta caricata, una riga per
/// ingresso (`…3.3.<riga>.<colonna>.0`). Verificata sulla T2600 il 28/10/2026
/// contro l'EWS (`DevMgmt/MediaHandlingDyn.xml`: Roll1 914.400000 mm, restano
/// 69552.065476 mm): la riga 1 è il rotolo, la riga 2 è vuota come il
/// MANUAL_FEED della MIB standard.
/// Colonne: 4 = nome supporto (stringa PML con prefisso binario), 10 = larghezza
/// e 11 = lunghezza residua in decipoint (1/720 di pollice).
const HP_CARTA: &str = "1.3.6.1.4.1.11.2.3.9.4.2.1.4.1.3.3";
const HP_COL_NOME: u32 = 4;
const HP_COL_LARGHEZZA: u32 = 10;
const HP_COL_RIMANENZA: u32 = 11;
/// MIB privata HP: percentuale d'inchiostro della cartuccia N in `…4.N.1.0`.
/// Il prtMarkerSuppliesLevel standard su questa stampante non è implementato.
const LIVELLI_HP: &str = "1.3.6.1.4.1.11.2.3.9.4.2.1.4.1.5.4";

#[derive(Clone, Debug, Default)]
pub struct Cartuccia {
    pub nome: String,
    pub colore: Option<&'static str>,
    pub percentuale: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct Ingresso {
    pub nome: String,
    pub stato: i64,
    /// Nome del supporto (es. "Plain Paper"), se la stampante lo espone.
    pub media: Option<String>,
    /// Larghezza in mm dalla MIB privata HP, se nota.
    pub larghezza_mm: Option<f64>,
    /// Carta residua sul rotolo in mm, se nota.
    pub rimanenti_mm: Option<f64>,
}

impl Ingresso {
    /// prtInputStatus è una maschera di bit (RFC 3805, PrtSubUnitStatusTC).
    pub fn descrizione(&self) -> &'static str {
        if self.stato & 32 != 0 {
            "offline"
        } else if self.stato & 16 != 0 {
            "vuoto / da caricare"
        } else {
            match self.stato & 7 {
                0 | 2 => "pronto",
                4 | 6 => "in uso",
                _ => "non disponibile",
            }
        }
    }

    pub fn pronto(&self) -> bool {
        self.stato & (16 | 32) == 0 && matches!(self.stato & 7, 0 | 2 | 4 | 6)
    }

    /// Testo per la GUI/CLI: "in uso — 914 mm (Plain Paper), restano 69.6 m".
    pub fn descrizione_lunga(&self) -> String {
        let mut s = self.descrizione().to_string();
        if let Some(mm) = self.larghezza_mm {
            s.push_str(&format!(" — {mm:.0} mm"));
        }
        if let Some(m) = self.media.as_deref().filter(|m| !m.is_empty()) {
            s.push_str(&format!(" ({m})"));
        }
        if let Some(r) = self.rimanenti_mm {
            if r >= 1000.0 {
                s.push_str(&format!(", restano {:.1} m", r / 1000.0));
            } else {
                s.push_str(&format!(", restano {r:.0} mm"));
            }
        }
        s
    }
}

#[derive(Clone, Debug, Default)]
pub struct StatoStampante {
    pub modello: Option<String>,
    pub stampante: Option<i64>,
    pub dispositivo: Option<i64>,
    pub cartucce: Vec<Cartuccia>,
    pub ingressi: Vec<Ingresso>,
}

impl StatoStampante {
    pub fn descrizione(&self) -> String {
        let base = match self.stampante {
            Some(3) => "pronta",
            Some(4) => "sta stampando",
            Some(5) => "in riscaldamento",
            _ => match self.dispositivo {
                Some(5) => "ferma (errore)",
                _ => "stato sconosciuto",
            },
        };
        match self.dispositivo {
            Some(3) => format!("{base} — con avvisi"),
            // Se la base viene già da hrDeviceStatus, l'errore è già detto.
            Some(5) if matches!(self.stampante, Some(3..=5)) => format!("{base} — errore"),
            _ => base.to_string(),
        }
    }

    /// true = tutto ok, false = avvisi o errori.
    pub fn ok(&self) -> bool {
        self.dispositivo == Some(2) && matches!(self.stampante, Some(3 | 4))
    }

    /// Larghezza del rotolo in mm con il nome dell'ingresso, dalla MIB privata HP.
    /// Preferisce un ingresso pronto il cui nome contiene "roll" (T2600: "ROLL"),
    /// altrimenti il primo ingresso pronto con larghezza nota, altrimenti il primo
    /// con larghezza nota. `None` = la stampante non espone il dato: la GUI usa
    /// il valore manuale come fallback.
    pub fn larghezza_rotolo_mm(&self) -> Option<(&str, f64)> {
        let con_larghezza =
            self.ingressi.iter().filter(|i| i.larghezza_mm.is_some_and(|m| m > 0.0));
        con_larghezza
            .clone()
            .filter(|i| i.pronto() && i.nome.to_ascii_uppercase().contains("ROLL"))
            .chain(con_larghezza.clone().filter(|i| i.pronto()))
            .chain(con_larghezza)
            .map(|i| (i.nome.as_str(), i.larghezza_mm.unwrap_or(0.0)))
            .next()
    }
}

/// Decipoint HP (1/720 di pollice) in mm. La T2600 espone 25920 per un rotolo
/// da 36" (25920/720*25,4 = 914,4) e 1971554 per i mm residui (69552 mm come
/// dice l'EWS); 0 o negativi (ingresso vuoto) non sono misure.
fn decipoint_in_mm(valore: i64) -> Option<f64> {
    if valore <= 0 {
        return None;
    }
    Some(valore as f64 * 25.4 / 720.0)
}

/// Larghezze plausibili per un rotolo DesignJet: fino a 3,5 m (oltre la 60").
fn larghezza_plausibile(valore: i64) -> Option<f64> {
    decipoint_in_mm(valore).filter(|m| *m <= 3500.0)
}

/// Stringhe della passerella PML di HP: due byte binari di testa (tipo e
/// lunghezza) prima del testo, che `from_utf8_lossy` mostra come "".
fn testo_pml(v: &Valore) -> Option<String> {
    let t = v
        .testo()?
        .trim_start_matches(|c: char| c == '\u{FFFD}' || c.is_whitespace() || c.is_control())
        .trim()
        .to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// Una riga della tabella carta HP (`HP_CARTA`), indicizzata come gli ingressi
/// della MIB standard (riga 1 = ROLL, riga 2 = MANUAL_FEED sulla T2600).
#[derive(Default)]
struct RigaHp {
    nome: Option<String>,
    larghezza_mm: Option<f64>,
    rimanenti_mm: Option<f64>,
}

/// Colore delle cartucce HP 730 dal codice prodotto (300 ml e 130 ml).
fn colore_hp730(nome: &str) -> Option<&'static str> {
    const CODICI: [(&str, &str); 12] = [
        ("P2V62A", "Ciano"),
        ("P2V63A", "Magenta"),
        ("P2V64A", "Giallo"),
        ("P2V65A", "Nero opaco"),
        ("P2V66A", "Grigio"),
        ("P2V67A", "Nero fotografico"),
        ("P2V68A", "Ciano"),
        ("P2V69A", "Magenta"),
        ("P2V70A", "Giallo"),
        ("P2V71A", "Nero opaco"),
        ("P2V72A", "Grigio"),
        ("P2V73A", "Nero fotografico"),
    ];
    CODICI.iter().find(|(c, _)| nome.contains(c)).map(|(_, n)| *n)
}

/// L'ultimo arco dell'OID è l'indice della riga.
fn per_indice(voci: Vec<(Vec<u32>, Valore)>) -> Vec<(u32, Valore)> {
    voci.into_iter().filter_map(|(o, v)| Some((*o.last()?, v))).collect()
}

pub fn leggi(host: &str) -> Result<StatoStampante> {
    leggi_da(host, 161)
}

pub fn leggi_da(host: &str, porta: u16) -> Result<StatoStampante> {
    let mut c = Client::new_con_porta(host, porta, "public")?;
    let mut stato = StatoStampante::default();

    for (o, v) in c.get(&[snmp::oid(MODELLO), snmp::oid(STATO_STAMPANTE), snmp::oid(STATO_DISPOSITIVO)])? {
        match o {
            o if o == snmp::oid(MODELLO) => stato.modello = v.testo(),
            o if o == snmp::oid(STATO_STAMPANTE) => stato.stampante = v.intero(),
            o if o == snmp::oid(STATO_DISPOSITIVO) => stato.dispositivo = v.intero(),
            _ => {}
        }
    }

    let livelli = c.walk(&snmp::oid(LIVELLI_HP)).unwrap_or_default();
    for (i, v) in per_indice(c.walk(&snmp::oid(NOMI_CARTUCCE))?) {
        let nome = v.testo().unwrap_or_default();
        let percentuale = livelli
            .iter()
            .find(|(o, _)| o.len() == snmp::oid(LIVELLI_HP).len() + 3 && o[o.len() - 3] == i && o[o.len() - 2] == 1)
            .and_then(|(_, v)| v.intero())
            .filter(|p| (0..=100).contains(p));
        stato.cartucce.push(Cartuccia { colore: colore_hp730(&nome), nome, percentuale });
    }

    let stati = per_indice(c.walk(&snmp::oid(STATI_INGRESSI))?);
    let media = per_indice(c.walk(&snmp::oid(MEDIA_INGRESSI)).unwrap_or_default());
    // La MIB standard sulla T2600 espone solo nomi e stati: le dimensioni
    // stanno nella tabella HP, che può mancare (vecchia stampante, SNMP
    // limitato) senza far fallire la lettura.
    let mut righe_hp: std::collections::HashMap<u32, RigaHp> = std::collections::HashMap::new();
    let base_hp = snmp::oid(HP_CARTA);
    for (o, v) in c.walk(&base_hp).unwrap_or_default() {
        if o.len() == base_hp.len() + 3 && o.starts_with(&base_hp) {
            let e = righe_hp.entry(o[base_hp.len()]).or_default();
            match o[base_hp.len() + 1] {
                HP_COL_NOME => e.nome = testo_pml(&v),
                HP_COL_LARGHEZZA => e.larghezza_mm = v.intero().and_then(larghezza_plausibile),
                HP_COL_RIMANENZA => e.rimanenti_mm = v.intero().and_then(decipoint_in_mm),
                _ => {}
            }
        }
    }
    for (i, v) in per_indice(c.walk(&snmp::oid(NOMI_INGRESSI))?) {
        let s = stati.iter().find(|(j, _)| *j == i).and_then(|(_, v)| v.intero()).unwrap_or(5);
        let hp = righe_hp.get(&i);
        let m = media
            .iter()
            .find(|(j, _)| *j == i)
            .and_then(|(_, v)| v.testo())
            .or_else(|| hp.and_then(|r| r.nome.clone()));
        stato.ingressi.push(Ingresso {
            nome: v.testo().unwrap_or_default(),
            stato: s,
            media: m,
            larghezza_mm: hp.and_then(|r| r.larghezza_mm),
            rimanenti_mm: hp.and_then(|r| r.rimanenti_mm),
        });
    }
    Ok(stato)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::snmp::test::{avvia_agente, ber, ber_intero};
    use std::collections::BTreeMap;
    use std::net::UdpSocket;
    use std::thread;
    use std::time::Duration;

    fn testo(s: &str) -> Vec<u8> {
        ber(0x04, s.as_bytes())
    }

    fn sotto(base: &str, indice: &str) -> Vec<u32> {
        snmp::oid(&format!("{base}.{indice}"))
    }

    /// Tabella che imita la T2600: modello, stati, due cartucce (una nota con
    /// livello, una ignota senza livello), rotolo in uso e manuale vuoto.
    fn tabella_base(stampante: i64, dispositivo: i64) -> BTreeMap<Vec<u32>, Vec<u8>> {
        BTreeMap::from([
            (snmp::oid(MODELLO), testo("HP DesignJet T2600")),
            (snmp::oid(STATO_STAMPANTE), ber_intero(stampante)),
            (snmp::oid(STATO_DISPOSITIVO), ber_intero(dispositivo)),
            (sotto(NOMI_CARTUCCE, "1"), testo("HP 730 300ML ( P2V62A )")),
            (sotto(NOMI_CARTUCCE, "2"), testo("HP 730 300ML ( P2V99A )")),
            (sotto(LIVELLI_HP, "1.1.0"), ber_intero(87)),
            (sotto(NOMI_INGRESSI, "1"), testo("ROLL")),
            (sotto(NOMI_INGRESSI, "2"), testo("MANUAL FEED")),
            (sotto(STATI_INGRESSI, "1"), ber_intero(6)),
            (sotto(STATI_INGRESSI, "2"), ber_intero(21)),
        ])
    }

    #[test]
    fn legge_stato_pronta() {
        let s = leggi_da("127.0.0.1", avvia_agente(tabella_base(3, 2))).unwrap();
        assert_eq!(s.modello.as_deref(), Some("HP DesignJet T2600"));
        assert_eq!(s.descrizione(), "pronta");
        assert!(s.ok());
        assert_eq!(s.ingressi.len(), 2);
        assert!(s.ingressi[0].pronto());
        assert_eq!(s.ingressi[1].descrizione(), "vuoto / da caricare");
        assert!(!s.ingressi[1].pronto());
        assert_eq!(s.cartucce.len(), 2);
        assert_eq!(s.cartucce[0].colore, Some("Ciano"));
        assert_eq!(s.cartucce[0].percentuale, Some(87));
        assert_eq!(s.cartucce[1].colore, None);
        assert_eq!(s.cartucce[1].percentuale, None);
    }

    #[test]
    fn legge_stato_errore() {
        let s = leggi_da("127.0.0.1", avvia_agente(tabella_base(1, 5))).unwrap();
        assert_eq!(s.descrizione(), "ferma (errore)");
        assert!(!s.ok());
    }

    #[test]
    fn inchiostro_esaurito_e_livelli_fuori_scala() {
        let mut t = tabella_base(3, 2);
        t.insert(sotto(LIVELLI_HP, "1.1.0"), ber_intero(0));
        t.insert(sotto(LIVELLI_HP, "2.1.0"), ber_intero(150));
        let s = leggi_da("127.0.0.1", avvia_agente(t)).unwrap();
        assert_eq!(s.cartucce[0].percentuale, Some(0));
        assert_eq!(s.cartucce[1].percentuale, None, "oltre 100 non è una percentuale");
    }

    #[test]
    fn nessuna_risposta_da_errore_leggibile() {
        // Agente muto: tiene la porta aperta senza rispondere (i datagrammi
        // restano in coda, niente ICMP), il client esaurisce i tentativi.
        // Dura ~4 s (2 tentativi × timeout 2 s): è il prezzo del test.
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let porta = sock.local_addr().unwrap().port();
        thread::spawn(move || {
            let _tiene_aperta = sock;
            thread::sleep(Duration::from_secs(6));
        });
        let err = leggi_da("127.0.0.1", porta).unwrap_err();
        assert!(format!("{err:#}").contains("nessuna risposta SNMP"), "{err:#}");
    }

    #[test]
    fn per_indice_usa_ultimo_arco() {
        let v = per_indice(vec![
            (vec![1, 2, 7], Valore::Intero(6)),
            (vec![1, 2, 3], Valore::Nullo),
            (vec![], Valore::Intero(1)),
        ]);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].0, 7);
        assert_eq!(v[0].1.intero(), Some(6));
    }

    #[test]
    fn descrizione_e_ok() {
        let pronta = StatoStampante { stampante: Some(3), dispositivo: Some(2), ..Default::default() };
        assert_eq!(pronta.descrizione(), "pronta");
        assert!(pronta.ok());
        let stampa = StatoStampante { stampante: Some(4), dispositivo: Some(2), ..Default::default() };
        assert_eq!(stampa.descrizione(), "sta stampando");
        assert!(stampa.ok());
        let avvisi = StatoStampante { stampante: Some(3), dispositivo: Some(3), ..Default::default() };
        assert_eq!(avvisi.descrizione(), "pronta — con avvisi");
        assert!(!avvisi.ok());
        let riscalda = StatoStampante { stampante: Some(5), dispositivo: Some(2), ..Default::default() };
        assert_eq!(riscalda.descrizione(), "in riscaldamento");
        assert!(!riscalda.ok());
        assert_eq!(StatoStampante::default().descrizione(), "stato sconosciuto");
        assert!(!StatoStampante::default().ok());
    }

    fn ingresso(nome: &str, stato: i64, larghezza_mm: Option<f64>) -> Ingresso {
        Ingresso { nome: nome.into(), stato, media: None, larghezza_mm, rimanenti_mm: None }
    }

    #[test]
    fn stati_ingressi_della_t2600() {
        // Valori letti dalla stampante: rotolo in uso, foglio singolo non caricato.
        assert_eq!(ingresso("ROLL", 6, None).descrizione(), "in uso");
        assert_eq!(ingresso("MANUAL_FEED", 21, None).descrizione(), "vuoto / da caricare");
    }

    #[test]
    fn colori_hp730() {
        assert_eq!(colore_hp730("HP 730 130ML ( P2V63A )"), Some("Magenta"));
        assert_eq!(colore_hp730("HP 730 300ML ( P2V73A )"), Some("Nero fotografico"));
        assert_eq!(colore_hp730("altro"), None);
    }

    #[test]
    fn decipoint_della_t2600() {
        // Rotolo da 36" letto dalla stampante: 25920/720*25,4 = 914,4 mm.
        assert!((decipoint_in_mm(25920).unwrap() - 914.4).abs() < 0.01);
        // Residuo letto: 1971554 decipoint ≈ 69552 mm, come dice l'EWS.
        assert!((decipoint_in_mm(1_971_554).unwrap() - 69552.04).abs() < 0.05);
        // Ingresso vuoto (0) e negativi non sono misure; il resto passa.
        assert_eq!(decipoint_in_mm(0), None);
        assert_eq!(decipoint_in_mm(-1), None);
        // Larghezza assurda (35 m) scartata, lunghezza residua no.
        assert_eq!(larghezza_plausibile(1_000_000), None);
        assert!((larghezza_plausibile(25920).unwrap() - 914.4).abs() < 0.01);
    }

    #[test]
    fn stringhe_pml_senza_prefisso_binario() {
        // La passerella PML di HP mette due byte binari davanti al testo.
        let v = Valore::Testo(vec![0x84, 0x0B, b'P', b'l', b'a', b'i', b'n']);
        assert_eq!(testo_pml(&v).as_deref(), Some("Plain"));
        assert_eq!(testo_pml(&Valore::Testo(vec![0x84, 0x0B])), None);
        assert_eq!(testo_pml(&Valore::Testo(b"  ".to_vec())), None);
        assert_eq!(testo_pml(&Valore::Intero(3)), None);
    }

    #[test]
    fn rotolo_rilevato_preferisce_il_roll_pronto() {
        let mut s = StatoStampante::default();
        s.ingressi.push(ingresso("MANUAL_FEED", 2, Some(297.0)));
        s.ingressi.push(ingresso("ROLL", 6, Some(914.4)));
        assert_eq!(s.larghezza_rotolo_mm(), Some(("ROLL", 914.4)));
    }

    #[test]
    fn senza_dimensioni_nessun_rotolo_rilevato() {
        let mut s = StatoStampante::default();
        s.ingressi.push(ingresso("ROLL", 6, None));
        assert_eq!(s.larghezza_rotolo_mm(), None);
        assert_eq!(StatoStampante::default().larghezza_rotolo_mm(), None);
    }
}

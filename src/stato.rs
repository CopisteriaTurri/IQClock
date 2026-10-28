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
            Some(5) if self.stampante.is_some() => format!("{base} — errore"),
            _ => base.to_string(),
        }
    }

    /// true = tutto ok, false = avvisi o errori.
    pub fn ok(&self) -> bool {
        self.dispositivo == Some(2) && matches!(self.stampante, Some(3 | 4))
    }
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
    for (i, v) in per_indice(c.walk(&snmp::oid(NOMI_INGRESSI))?) {
        let s = stati.iter().find(|(j, _)| *j == i).and_then(|(_, v)| v.intero()).unwrap_or(5);
        stato.ingressi.push(Ingresso { nome: v.testo().unwrap_or_default(), stato: s });
    }
    Ok(stato)
}

#[cfg(test)]
mod test {
    use super::*;
    use std::collections::BTreeMap;
    use std::net::UdpSocket;
    use std::thread;
    use std::time::Duration;

    // --- Mini-codec BER solo per il finto agente. Duplicato volutamente:
    // così non si toccano i privati di snmp.rs. ---

    fn tlv(tag: u8, contenuto: &[u8]) -> Vec<u8> {
        let mut o = vec![tag];
        if contenuto.len() < 0x80 {
            o.push(contenuto.len() as u8);
        } else {
            let b: Vec<u8> =
                contenuto.len().to_be_bytes().into_iter().skip_while(|&x| x == 0).collect();
            o.push(0x80 | b.len() as u8);
            o.extend_from_slice(&b);
        }
        o.extend_from_slice(contenuto);
        o
    }

    fn grezzo_intero(v: i64) -> Vec<u8> {
        let b = v.to_be_bytes();
        let mut i = 0;
        while i < 7 && ((b[i] == 0 && b[i + 1] & 0x80 == 0) || (b[i] == 0xFF && b[i + 1] & 0x80 != 0))
        {
            i += 1;
        }
        b[i..].to_vec()
    }

    fn codifica_oid(oid: &[u32]) -> Vec<u8> {
        let mut c = vec![(oid[0] * 40 + oid[1]) as u8];
        for &arco in &oid[2..] {
            let mut g = vec![(arco & 0x7F) as u8];
            let mut v = arco >> 7;
            while v > 0 {
                g.push((v & 0x7F) as u8 | 0x80);
                v >>= 7;
            }
            c.extend(g.into_iter().rev());
        }
        tlv(0x06, &c)
    }

    fn decodifica_intero(c: &[u8]) -> i64 {
        let iniziale = if c.first().is_some_and(|b| b & 0x80 != 0) { -1 } else { 0 };
        c.iter().fold(iniziale, |a, &b| a << 8 | b as i64)
    }

    fn decodifica_oid(c: &[u8]) -> Vec<u32> {
        let Some((&primo, resto)) = c.split_first() else { return Vec::new() };
        let mut o = vec![(primo / 40) as u32, (primo % 40) as u32];
        let mut v = 0u32;
        for &b in resto {
            v = v << 7 | (b & 0x7F) as u32;
            if b & 0x80 == 0 {
                o.push(v);
                v = 0;
            }
        }
        o
    }

    struct L<'a>(&'a [u8]);

    impl<'a> L<'a> {
        fn un(&mut self) -> (u8, &'a [u8]) {
            let d = self.0;
            let tag = d[0];
            let (n, s) = if d[1] & 0x80 == 0 {
                (d[1] as usize, 2)
            } else {
                let k = (d[1] & 0x7F) as usize;
                (d[2..2 + k].iter().fold(0usize, |a, &b| a << 8 | b as usize), 2 + k)
            };
            self.0 = &d[s + n..];
            (tag, &d[s..s + n])
        }
    }

    /// (id, è_getnext, oids) dalla richiesta del client.
    fn leggi_richiesta(d: &[u8]) -> (i64, bool, Vec<Vec<u32>>) {
        let mut m = L(d);
        let (_, c) = m.un();
        let mut m = L(c);
        let _ = m.un(); // versione
        let _ = m.un(); // comunità
        let (pdu, pc) = m.un();
        let mut p = L(pc);
        let (_, idc) = p.un();
        let _ = p.un();
        let _ = p.un();
        let (_, vbc) = p.un();
        let mut v = L(vbc);
        let mut oids = Vec::new();
        while !v.0.is_empty() {
            let (_, s) = v.un();
            let mut s = L(s);
            let (_, oc) = s.un();
            oids.push(decodifica_oid(oc));
        }
        (decodifica_intero(idc), pdu == 0xA1, oids)
    }

    fn risposta(id: i64, errore: i64, voci: &[(Vec<u32>, u8, Vec<u8>)]) -> Vec<u8> {
        let vb: Vec<u8> = voci
            .iter()
            .flat_map(|(o, t, c)| tlv(0x30, &[codifica_oid(o), tlv(*t, c)].concat()))
            .collect();
        let pdu =
            [tlv(0x02, &grezzo_intero(id)), tlv(0x02, &grezzo_intero(errore)), tlv(0x02, &[0]), tlv(0x30, &vb)]
                .concat();
        tlv(0x30, &[tlv(0x02, &[0]), tlv(0x04, b"public"), tlv(0xA2, &pdu)].concat())
    }

    /// Agente SNMP finto su 127.0.0.1: risponde da una tabella OID →
    /// (tag, valore). GETNEXT restituisce la prima chiave maggiore (le walk
    /// del client scartano da sole ciò che esce dalla base); OID ignoti →
    /// errore 2 come un agente vero. Il thread muore da solo dopo 0,5 s
    /// di inattività: niente join nei test.
    fn avvia_agente(tabella: BTreeMap<Vec<u32>, (u8, Vec<u8>)>) -> u16 {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let porta = sock.local_addr().unwrap().port();
        thread::spawn(move || {
            sock.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
            let mut buf = vec![0u8; 65535];
            for _ in 0..400 {
                let (n, da) = match sock.recv_from(&mut buf) {
                    Ok(r) => r,
                    Err(_) => break,
                };
                let (id, next, oids) = leggi_richiesta(&buf[..n]);
                let r = if next {
                    tabella
                        .range::<[u32], _>((
                            std::ops::Bound::Excluded(oids[0].as_slice()),
                            std::ops::Bound::Unbounded,
                        ))
                        .next()
                        .map(|(o, (t, c))| risposta(id, 0, &[(o.clone(), *t, c.clone())]))
                        .unwrap_or_else(|| risposta(id, 2, &[]))
                } else {
                    let voci: Vec<_> = oids
                        .iter()
                        .filter_map(|o| tabella.get(o).map(|(t, c)| (o.clone(), *t, c.clone())))
                        .collect();
                    if voci.len() == oids.len() {
                        risposta(id, 0, &voci)
                    } else {
                        risposta(id, 2, &[])
                    }
                };
                let _ = sock.send_to(&r, da);
            }
        });
        porta
    }

    /// Tabella che imita la T2600: modello, stati, due cartucce (una nota con
    /// livello, una ignota senza livello), rotolo in uso e manuale vuoto.
    fn tabella_base(stampante: i64, dispositivo: i64) -> BTreeMap<Vec<u32>, (u8, Vec<u8>)> {
        let mut t: BTreeMap<Vec<u32>, (u8, Vec<u8>)> = BTreeMap::new();
        t.insert(snmp::oid(MODELLO), (0x04, b"HP DesignJet T2600".to_vec()));
        t.insert(snmp::oid(STATO_STAMPANTE), (0x02, grezzo_intero(stampante)));
        t.insert(snmp::oid(STATO_DISPOSITIVO), (0x02, grezzo_intero(dispositivo)));
        t.insert(
            snmp::oid(&format!("{NOMI_CARTUCCE}.1")),
            (0x04, b"HP 730 300ML ( P2V62A )".to_vec()),
        );
        t.insert(
            snmp::oid(&format!("{NOMI_CARTUCCE}.2")),
            (0x04, b"HP 730 300ML ( P2V99A )".to_vec()),
        );
        t.insert(snmp::oid(&format!("{LIVELLI_HP}.1.1.0")), (0x02, grezzo_intero(87)));
        t.insert(snmp::oid(&format!("{NOMI_INGRESSI}.1")), (0x04, b"ROLL".to_vec()));
        t.insert(snmp::oid(&format!("{NOMI_INGRESSI}.2")), (0x04, b"MANUAL FEED".to_vec()));
        t.insert(snmp::oid(&format!("{STATI_INGRESSI}.1")), (0x02, grezzo_intero(6)));
        t.insert(snmp::oid(&format!("{STATI_INGRESSI}.2")), (0x02, grezzo_intero(21)));
        t
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
        assert_eq!(s.descrizione(), "ferma (errore) — errore");
        assert!(!s.ok());
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

    #[test]
    fn stati_ingressi_della_t2600() {
        // Valori letti dalla stampante: rotolo in uso, foglio singolo non caricato.
        assert_eq!(Ingresso { nome: "ROLL".into(), stato: 6 }.descrizione(), "in uso");
        assert_eq!(Ingresso { nome: "MANUAL_FEED".into(), stato: 21 }.descrizione(), "vuoto / da caricare");
    }

    #[test]
    fn colori_hp730() {
        assert_eq!(colore_hp730("HP 730 130ML ( P2V63A )"), Some("Magenta"));
        assert_eq!(colore_hp730("HP 730 300ML ( P2V73A )"), Some("Nero fotografico"));
        assert_eq!(colore_hp730("altro"), None);
    }
}

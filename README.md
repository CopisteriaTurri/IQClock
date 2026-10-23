# HP Clock

Alternativa minima a HP Click per la DesignJet T2600 PostScript (e in generale per i
DesignJet con interprete PostScript/PDF). Trascini i file, scegli le opzioni, premi
**Stampa**: il file viene mandato così com'è alla porta 9100 con un'intestazione PJL.
Il RIP lo fa la stampante, quindi non serve il motore di HP Click.

## Uso

- **Interfaccia**: `hpclock.exe` (oppure trascina i file sull'eseguibile).
- **Riga di comando** (su Windows l'output è visibile solo dalla build di debug):
  ```
  hpclock stampa tavola.pdf --copie 2 --rotolo 1 --grigi
  hpclock stampa tavola.pdf --prn prova.prn     # scrive il job su file senza stampare
  hpclock stato                                  # stato, inchiostri, carta
  hpclock snmp 1.3.6.1.2.1.43                    # diagnostica SNMP
  ```

Le impostazioni (IP, porta, larghezza rotolo) stanno in `%APPDATA%\hpclock\config.txt`.
Di default: `192.168.10.72:9100`, rotolo 36".

## Cosa viene mandato alla stampante

```
<ESC>%-12345X@PJL JOB NAME="tavola.pdf"
@PJL SET COPIES=2
@PJL SET MEDIASOURCE=ROLL1        (solo se scelto)
@PJL SET RENDERMODE=COLOR|GRAYSCALE
@PJL SET CUTTER=ON|OFF            (solo se scelto)
@PJL ENTER LANGUAGE=PDF           (POSTSCRIPT, HPGL2; omesso per TIFF/JPEG)
...file...
<ESC>%-12345X@PJL EOJ NAME="tavola.pdf"
<ESC>%-12345X
```

| Comando | Fonte | Verificato sulla T2600 |
|---|---|---|
| UEL, JOB/EOJ, ENTER LANGUAGE, COPIES, RENDERMODE | HP PJL Technical Reference Manual (2003) | no |
| MEDIASOURCE=ROLL1/ROLL2/AUTO | tabella PJL dei DesignJet XL 3600 | no |
| CUTTER=ON/OFF | driver CUPS per DesignJet, non documentato da HP | no |
| `@PJL INFO STATUS` | manuale PJL | **non supportato**: risponde `"?"` |

Prima di fidarsi delle opzioni, conviene stampare una volta da HP Click o dal driver
Windows "su file" e confrontare l'intestazione PJL che usano.

## Stato della stampante (SNMP v1, community `public`)

Verificato sulla T2600 il 23/10/2026:

| Dato | OID |
|---|---|
| Modello | `1.3.6.1.2.1.25.3.2.1.3.1` (hrDeviceDescr) |
| Stato stampante (3 pronta, 4 stampa, 5 riscaldamento) | `1.3.6.1.2.1.25.3.5.1.1.1` |
| Stato dispositivo (2 ok, 3 avvisi, 5 errore) | `1.3.6.1.2.1.25.3.2.1.5.1` |
| Nomi cartucce | `1.3.6.1.2.1.43.11.1.1.6.1.N` |
| % inchiostro (MIB privata HP) | `1.3.6.1.4.1.11.2.3.9.4.2.1.4.1.5.4.N.1.0` |
| Ingressi carta / stato | `1.3.6.1.2.1.43.8.2.1.13.1.N` / `….11.1.N` |

Il `prtMarkerSuppliesLevel` standard non è implementato dalla stampante.

## Limiti del prototipo

- Anteprima PDF solo come sagoma in scala sul rotolo (formato e numero di pagine);
  JPEG e TIFF si vedono davvero. Per il rendering PDF serve pdfium (`pdfium.dll`).
- Niente nesting, ridimensionamento o rotazione automatica: decide la stampante.
- Le immagini sono disegnate supponendo 300 dpi.
- Nessun controllo che il job sia stato effettivamente stampato: "inviato" vuol dire
  che la stampante ha ricevuto tutti i byte.

## Compilare

```
cargo build --release                                  # Linux
cargo build --release --target x86_64-pc-windows-gnu   # Windows (serve mingw64-gcc)
cargo test
```

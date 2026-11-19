# Changelog

Le modifiche che cambiano il comportamento osservabile, l'API pubblica o la
politica delle dipendenze. La versione dei manifest resta quella dell'ultima
release finché una release non viene preparata.

## Non rilasciato

- Supporto limitato a Linux e Windows, con errore di compilazione altrove.
- Copertura completa e campagne operative/fuzz solo su avvio manuale.

## 0.5.0 (2026-11-15)

- Rimozioni incompatibili: eliminati il freeze storico delle superfici pubbliche,
  la baseline di compatibilità e il comando per rigenerarla. Una sola versione
  corrente per famiglia di schema e binding locale, verificata dal validatore.
- Eliminati changelog pre-0.4.0 e ricevute delle campagne su commit precedenti;
  restano harness, fixture, oracoli e prove ABI3/Python 3.14 correnti.
- Allineati tutti i manifesti dei contratti al pin di famiglia; registrata la
  non applicabilità dei nuovi contratti Arrow/geometria e scambio file a REST.
  La deviazione RT-024 resta aperta. (#57)
- Semplificazione interna a comportamento invariato: catalogo unico delle tuple
  di errore e reducer degli eventi; RecordPlan validato e trasformazione unica
  per executor seriale, concorrente e batch; controllo condiviso di cancellazione
  e deadline; decisore retry unico e separazione di engine e transport in moduli
  per responsabilità. (#56)
- Prove della ristrutturazione: oracoli indipendenti per transizioni, ordine,
  batch e retry; iniezione di cancellazione, deadline e guasti in 19 punti;
  sentinelle dei segreti, segnali CLI e verifica tramite mutazioni. Identità
  delle fixture canoniche LF anche su Windows e ambienti di prova autonomi. (#55)
- Un solo risolutore della CLI di prova per le suite della wheel installata
  in verify, release e coverage: ricava l'eseguibile dai metadata Cargo,
  ne verifica l'esistenza e passa PLENORA_TEST_CLI direttamente alla suite. (#55)
- Aritmetica temporale checked condivisa: TTL OAuth e ArcGIS estremi producono
  errori tipizzati; timer fuori orizzonte rifiutati prima della rete.
- Retry-After enorme impedisce il retry anche con limite configurato massimo;
  le date HTTP arrotondano l'attesa per eccesso. Max-age della cache satura
  esplicitamente. Controllati anche cookie Expires/Max-Age, deadline, timeout
  e moltiplicazioni del backoff. Guardia Clippy sui tipi temporali e corpus
  fuzz ampliato. (#58)

## 0.4.0 (2026-11-14)

- Cambio incompatibile nella serie 0.x, deciso dal titolare: Python minimo
  3.14 e wheel ABI3 cp314. Python precedenti non sono più supportati.
  `.python-version` è la fonte verificata dai gate per CI e rilascio.
- SDK asincrono pubblico `AsyncEngine`, derivato da `Engine`: stesse operazioni,
  sessioni persistenti, cancellazione cooperativa e attesa della fine del worker.
- Prima ondata di allineamento alla politica comune delle dipendenze Plenora,
  con snapshot verificato in CI e parser aggiornato a quick-xml 0.42.

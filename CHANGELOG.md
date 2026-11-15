# Changelog

Le modifiche che cambiano il comportamento osservabile, l'API pubblica o la
politica delle dipendenze. La versione dei manifest resta quella dell'ultima
release finché una release non viene preparata.

## Non rilasciata

- Rimozioni incompatibili: eliminati il freeze storico delle superfici pubbliche,
  la baseline di compatibilità e il comando per rigenerarla. Una sola versione
  corrente per famiglia di schema e binding locale, verificata dal validatore.
- Eliminati changelog pre-0.4.0 e ricevute delle campagne su commit precedenti;
  restano harness, fixture, oracoli e prove ABI3/Python 3.14 correnti.
- Allineati tutti i manifesti dei contratti al pin di famiglia; registrata la
  non applicabilità dei nuovi contratti Arrow/geometria e scambio file a REST.
  La deviazione RT-024 resta aperta. Versioni e digest di release restano quelli
  registrati fino alla preparazione e qualifica della prossima release.

## 0.4.0 (2026-11-14)

- Cambio incompatibile nella serie 0.x, deciso dal titolare: Python minimo
  3.14 e wheel ABI3 cp314. Python precedenti non sono più supportati.
  `.python-version` è la fonte verificata dai gate per CI e rilascio.
- SDK asincrono pubblico `AsyncEngine`, derivato da `Engine`: stesse operazioni,
  sessioni persistenti, cancellazione cooperativa e attesa della fine del worker.
- Prima ondata di allineamento alla politica comune delle dipendenze Plenora,
  con snapshot verificato in CI e parser aggiornato a quick-xml 0.42.

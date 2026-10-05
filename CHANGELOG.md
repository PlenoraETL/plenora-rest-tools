# Changelog

Le modifiche che cambiano il comportamento osservabile, l'API pubblica o la
politica delle dipendenze. La versione dei manifest resta quella dell'ultima
release finché una release non viene preparata.

## Non rilasciato

### Comportamento

- Un null esplicito in value di un parametro, di una trasformazione o in
  default di un output_mapping non è più confuso con l'assenza del campo. Un
  parametro fixed con value null invia null nel body JSON; un parametro fixed
  senza value è rifiutato con INVALID_INPUT. Vedi
  [Null e valori assenti](docs/architecture.md#null-e-valori-assenti).
- Un null in path, query, header, cookie, form, multipart o template raw è
  rifiutato con INVALID_INPUT invece di essere inviato come stringa vuota.
- Null non è più letto come stringa vuota nemmeno nei valori della risposta:
  prefix, suffix e replace su null restituiscono null; una condition su una
  colonna null non si applica; uno status di polling null è INVALID_RESPONSE;
  un job id null è un job id assente. Un value null in una trasformazione
  diversa da default_if_null è INVALID_INPUT.

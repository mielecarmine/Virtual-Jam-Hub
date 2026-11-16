See `.rust-skills/AGENTS.md` for Rust development guidelines.

## Flusso obbligatorio per i ticket JAM

Quando l'utente chiede di partire con un nuovo ticket, seguire sempre questo ordine:

1. Leggere il ticket completo nella JAM board collegata alla repository.
2. Spostare il ticket in `In progress`.
3. Aprire un branch dedicato da `develop` aggiornato (prefisso `codex/`).
4. Implementare il ticket ed eseguire i test e tutti i gate di verifica del progetto.
5. Presentare il risultato e attendere l'approvazione esplicita dell'utente prima del merge.
6. Dopo l'approvazione, effettuare il merge manuale su `develop`.
7. Spostare il ticket in `Done` solo dopo il merge.

Se un permesso impedisce un passaggio, segnalarlo senza dichiararlo completato.

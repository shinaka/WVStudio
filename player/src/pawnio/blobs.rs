//! The module files: finding the module folder from the player's own path (`<exe dir>\pawnio\`, else the
//! repo's `player\pawnio\` when it runs from `player\target\<profile>\`), never from the working folder,
//! and reading a module, refusing one whose SHA-256 isn't the one pinned here.

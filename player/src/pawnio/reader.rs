//! The reader: opens the modules, reads the CPU and then the chips, and makes every failure a note, so the
//! rest still comes through. Every hardware sequence holds one lock in the process, so whoever ends the
//! process lets the sequence finish, and put back what it changed, first.

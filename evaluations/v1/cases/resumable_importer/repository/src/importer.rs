pub fn run(source: &Source, database: &Database) {
    for batch in source.batches() {
        database.insert_batch(batch);
    }
}

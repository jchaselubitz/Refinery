pub fn transaction<T>(work: impl FnOnce(&mut Connection) -> T) -> T {
    connection.transaction(work)
}

"""Read catalog shape or make a consistent copy without booting an Inseam node."""
import json
import pathlib
import sqlite3
import sys


def connect(path):
    database = sqlite3.connect(pathlib.Path(path).resolve().as_uri() + '?mode=ro', uri=True, timeout=5)
    database.row_factory = sqlite3.Row
    # Limit VM work as well as the caller's subprocess deadline.
    calls = 0

    def progress():
        nonlocal calls
        calls += 1
        return int(calls > 200000)

    database.set_progress_handler(progress, 10000)
    return database


def rows(database, query, parameters=()):
    return [dict(row) for row in database.execute(query, parameters).fetchmany(513)]


def overview(database):
    totals = {table: database.execute('SELECT count(*) FROM ' + table).fetchone()[0]
              for table in ['sources', 'fragments', 'relations', 'search_rows', 'keyed_fragments']}
    totals['indexed'] = database.execute('SELECT count(*) FROM sources WHERE indexed = 1').fetchone()[0]
    types = rows(database, 'SELECT mimetype AS name, count(*) AS count FROM fragments GROUP BY mimetype ORDER BY count DESC LIMIT 128')
    edges = rows(database, '''SELECT a.mimetype AS source, r.kind, b.mimetype AS target, count(*) AS count
        FROM relations r JOIN fragments a ON a.id = r.from_fragment
        JOIN fragments b ON b.id = r.to_fragment
        GROUP BY a.mimetype, r.kind, b.mimetype ORDER BY count DESC LIMIT 256''')
    return {'totals': totals, 'types': types, 'edges': edges}


def catalog(database, options):
    term = options.get('term', '')
    content_type = options.get('contentType', '')
    offset = int(options.get('offset', 0))
    if not 0 <= offset <= 10000000:
        raise ValueError('Catalog offset out of range')
    clause = 'WHERE instr(locator, ?) > 0 AND (? = \'\' OR content_type = ?)'
    parameters = (term, content_type, content_type)
    count = database.execute('SELECT count(*) FROM sources ' + clause, parameters).fetchone()[0]
    entries = rows(database, 'SELECT host, locator, indexed, content_type, raw_bytes FROM sources ' + clause
                   + ' ORDER BY host, locator LIMIT 100 OFFSET ?', parameters + (offset,))
    return {'entries': entries, 'count': count, 'offset': offset}


def main():
    operation, source = sys.argv[1:3]
    with connect(source) as database:
        if operation == 'backup':
            with sqlite3.connect(sys.argv[3], timeout=5) as destination:
                database.backup(destination, pages=1024, sleep=0.01)
            result = {'copied': True}
        elif operation == 'overview':
            result = overview(database)
        elif operation == 'catalog':
            result = catalog(database, json.loads(sys.argv[3]))
        else:
            raise ValueError('Unknown database operation')
    print(json.dumps(result))


if __name__ == '__main__':
    main()

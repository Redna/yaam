import unittest
import os
import ladybug as lb
from db import setup_database, get_connection, DB_PATH

class TestDatabase(unittest.TestCase):
    def setUp(self):
        # Use a separate test database
        self.test_db_path = "./test_memory.lbug"
        if os.path.exists(self.test_db_path):
            import shutil
            if os.path.isdir(self.test_db_path):
                shutil.rmtree(self.test_db_path)
            else:
                os.remove(self.test_db_path)
        
        # Patch DB_PATH in db module
        import db
        db.DB_PATH = self.test_db_path
        setup_database()

    def tearDown(self):
        if os.path.exists(self.test_db_path):
            import shutil
            if os.path.isdir(self.test_db_path):
                shutil.rmtree(self.test_db_path)
            else:
                os.remove(self.test_db_path)

    def test_schema_creation(self):
        conn = get_connection()
        # Verify tables exist by attempting a simple select
        try:
            conn.execute("MATCH (e:Entity) RETURN count(*)")
            conn.execute("MATCH (w:Workspace) RETURN count(*)")
            conn.execute("MATCH (s:Scratchpad) RETURN count(*)")
        except Exception as e:
            self.fail(f"Schema verification failed: {e}")

if __name__ == "__main__":
    unittest.main()

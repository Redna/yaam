import unittest
import os
import shutil
import db
import server
from logger import HISTORY_FILE

class TestServer(unittest.TestCase):
    def setUp(self):
        self.test_db_path = "./test_server.lbug"
        if os.path.exists(self.test_db_path):
            if os.path.isdir(self.test_db_path):
                shutil.rmtree(self.test_db_path)
            else:
                os.remove(self.test_db_path)
        
        db.DB_PATH = self.test_db_path
        db.setup_database()
        
        # Clear history
        if os.path.exists(HISTORY_FILE):
            os.remove(HISTORY_FILE)

    def tearDown(self):
        if os.path.exists(self.test_db_path):
            if os.path.isdir(self.test_db_path):
                shutil.rmtree(self.test_db_path)
            else:
                os.remove(self.test_db_path)
        if os.path.exists(HISTORY_FILE):
            os.remove(HISTORY_FILE)

    def test_workspace_tools(self):
        # 1. Initialize workspace
        resp = server.workspace_initialize("test_ws", "Unit test workspace")
        self.assertIn("initialized successfully", resp)
        
        # 2. Add note
        resp = server.workspace_append_note("test_ws", "Test note content")
        self.assertIn("Note added", resp)
        
        # 3. Verify in DB
        conn = db.get_connection()
        res = conn.execute("MATCH (w:Workspace {workspace_name: 'test_ws'})-[:HAS_SCRATCHPAD]->(s:Scratchpad) RETURN s.content")
        self.assertTrue(res.has_next())
        self.assertEqual(res.get_next()[0], "Test note content")
        
        # 4. Verify History Log
        with open(HISTORY_FILE, "r") as f:
            lines = f.readlines()
            self.assertEqual(len(lines), 2)

    def test_graph_explore_guard(self):
        # Attempt a write operation via explore
        resp = server.graph_explore("CREATE (n:Entity {id: 'hack'})")
        self.assertIn("ERROR: Write operations forbidden", resp)

if __name__ == "__main__":
    unittest.main()

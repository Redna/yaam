import unittest
import os
import shutil
import db
import server
from reconciler import reconcile

class TestAgentScenario(unittest.TestCase):
    """
    Simulates a sequence of events typical for an agent session:
    1. Session starts (setup)
    2. Agent initializes a workspace
    3. Agent creates a new file (e.g. implementing a feature)
    4. PostToolUse hook fires (reconcile)
    5. Agent adds a scratchpad note about the implementation
    6. Agent queries the graph to verify state
    """
    def setUp(self):
        self.test_db_path = "./test_scenario.lbug"
        if os.path.exists(self.test_db_path):
            if os.path.isdir(self.test_db_path):
                shutil.rmtree(self.test_db_path)
            else:
                os.remove(self.test_db_path)
        
        db.DB_PATH = self.test_db_path
        db.setup_database()

    def tearDown(self):
        if os.path.exists(self.test_db_path):
            if os.path.isdir(self.test_db_path):
                shutil.rmtree(self.test_db_path)
            else:
                os.remove(self.test_db_path)

    def test_end_to_end_agent_loop(self):
        # 1. Agent initializes workspace
        server.mcp__workspace__initialize("auth_fix", "Fixing login leak")
        
        # 2. Agent creates a new file (simulated tool execution)
        test_file = "auth_handler.py"
        with open(test_file, "w") as f:
            f.write("def login():\n    pass\n")
        
        try:
            # 3. PostToolUse hook fires
            reconcile()
            
            # 4. Agent adds a note
            server.mcp__workspace__append_note("auth_fix", "Implemented login stub in auth_handler.py")
            
            # 5. Agent queries the graph
            conn = db.get_connection()
            # Verify file entity
            res = conn.execute(f"MATCH (e:Entity {{id: '{test_file}'}}) RETURN e.status")
            self.assertTrue(res.has_next())
            self.assertEqual(res.get_next()[0], "active")
            
            # Verify scratchpad link
            res = conn.execute("MATCH (w:Workspace {workspace_name: 'auth_fix'})-[:HAS_SCRATCHPAD]->(s:Scratchpad) RETURN s.content")
            self.assertTrue(res.has_next())
            self.assertEqual(res.get_next()[0], "Implemented login stub in auth_handler.py")
            
            # Verify function extraction
            res = conn.execute(f"MATCH (e:Entity {{id: '{test_file}::login'}}) RETURN e.type")
            self.assertTrue(res.has_next())
            
        finally:
            if os.path.exists(test_file):
                os.remove(test_file)

if __name__ == "__main__":
    unittest.main()

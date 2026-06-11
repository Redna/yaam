import unittest
import os
import time
import shutil
import db
from reconciler import reconcile

class TestReconciler(unittest.TestCase):
    def setUp(self):
        self.test_db_path = "./test_reconcile.lbug"
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

    def test_reconcile_integration(self):
        # 1. Create a dummy Python file
        test_file = "reconcile_test.py"
        with open(test_file, "w") as f:
            f.write("def dummy_func():\n    pass\n")
        
        try:
            # 2. Run reconcile (it uses git status --porcelain)
            # Since this file is untracked, it should show up as '??'
            reconcile()
            
            # 3. Verify it's in the DB
            conn = db.get_connection()
            res = conn.execute(f"MATCH (e:Entity {{id: '{test_file}'}}) RETURN e.type, e.status")
            self.assertTrue(res.has_next())
            row = res.get_next()
            self.assertEqual(row[0], "File")
            self.assertEqual(row[1], "active")
            
            # 4. Verify function was extracted
            func_id = f"{test_file}::dummy_func"
            res = conn.execute(f"MATCH (f:Entity {{id: '{func_id}'}}) RETURN f.type")
            self.assertTrue(res.has_next())
            
        finally:
            if os.path.exists(test_file):
                os.remove(test_file)

if __name__ == "__main__":
    unittest.main()

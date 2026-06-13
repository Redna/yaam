import unittest
import asyncio
import os
import shutil
import db
import server
from reconciler import reconcile
from logger import HISTORY_FILE

class TestMCPReal(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        # 1. Use a dedicated integration test database
        self.test_db_path = "./test_mcp_real.lbug"
        if os.path.exists(self.test_db_path):
            if os.path.isdir(self.test_db_path):
                shutil.rmtree(self.test_db_path)
            else:
                os.remove(self.test_db_path)
        
        # Patch the database path
        db.DB_PATH = self.test_db_path
        db.setup_database()
        
        # Clear history
        if os.path.exists(HISTORY_FILE):
            os.remove(HISTORY_FILE)

    async def asyncTearDown(self):
        if os.path.exists(self.test_db_path):
            if os.path.isdir(self.test_db_path):
                shutil.rmtree(self.test_db_path)
            else:
                os.remove(self.test_db_path)
        if os.path.exists(HISTORY_FILE):
            os.remove(HISTORY_FILE)

    async def test_full_mcp_lifecycle(self):
        """
        Tests the full lifecycle using the actual FastMCP tool interface.
        """
        # 1. Initialize Workspace via MCP
        # FastMCP.call_tool returns a ToolResult object or similar depending on version
        # Prefect FastMCP returns the actual result of the function if called directly 
        # or a ToolResult if using the protocol.
        
        ws_name = "integration_test_ws"
        ws_desc = "Testing real MCP tools"
        
        # Call workspace_initialize
        resp = await server.mcp.call_tool("workspace_initialize", {"name": ws_name, "description": ws_desc})
        self.assertIn("initialized successfully", str(resp))
        
        # 2. Add a note with complex characters (proving robustness)
        complex_note = "Decision: Use 'LadybugDB' for Layer 0. It's fast!"
        resp = await server.mcp.call_tool("workspace_append_note", {
            "workspace_name": ws_name, 
            "content": complex_note
        })
        self.assertIn("Note added", str(resp))
        
        # 3. Create a real file to trigger physical layer sync
        test_file = "integration_sample.py"
        with open(test_file, "w") as f:
            f.write("def integration_func():\n    return True\n")
        
        try:
            # Run Reconciler (simulating AfterTool hook)
            reconcile()
            
            # 4. Use graph_explore via MCP to verify the join between Layer 0 and Layer 1
            query = f"MATCH (w:Workspace {{workspace_name: '{ws_name}'}})-[:HAS_SCRATCHPAD]->(s:Scratchpad), (e:Entity {{id: '{test_file}'}}) RETURN s.content, e.status"
            
            # Use MCP tool to explore
            resp = await server.mcp.call_tool("graph_explore", {"query": query})
            
            # Access the text content directly from ToolResult
            found_note = False
            for item in resp.content:
                if hasattr(item, "text") and complex_note in item.text:
                    found_note = True
                    break
            
            if not found_note:
                self.assertIn("Decision: Use", str(resp))
                self.assertIn("LadybugDB", str(resp))
            
            # 5. Verify function extraction also worked
            func_query = f"MATCH (f:Entity {{id: '{test_file}::integration_func'}}) RETURN f.type"
            resp = await server.mcp.call_tool("graph_explore", {"query": func_query})
            resp_str = str(resp)
            self.assertIn("Function", resp_str)
            
        finally:
            if os.path.exists(test_file):
                os.remove(test_file)

if __name__ == "__main__":
    unittest.main()

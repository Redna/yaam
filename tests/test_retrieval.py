import unittest
import time
from retrieval import evaluate_scratchpad_decay

class TestRetrieval(unittest.TestCase):
    def test_decay_logic(self):
        now = time.time()
        # Note created 1 hour ago
        fresh_note = {"id": "1", "content": "fresh", "created_at": now - 3600}
        # Note created 50 hours ago (should be below 0.1 threshold)
        # exp(-0.05 * 50) = 0.082
        stale_note = {"id": "2", "content": "stale", "created_at": now - 50 * 3600}
        
        notes = [fresh_note, stale_note]
        active = evaluate_scratchpad_decay(notes, decay_constant=0.05)
        
        self.assertEqual(len(active), 1)
        self.assertEqual(active[0]["id"], "1")
        self.assertGreater(active[0]["recency_weight"], 0.9)

if __name__ == "__main__":
    unittest.main()

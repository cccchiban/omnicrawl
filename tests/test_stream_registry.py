"""活跃模型流注册表的轻量回归测试。"""







from __future__ import annotations







import threading
import unittest
from concurrent.futures import ThreadPoolExecutor
from contextvars import copy_context







from omnicrawl.llm import stream_registry











class StreamRegistryTest(unittest.TestCase):



    def test_register_close_and_unregister_cycle(self) -> None:



        closed: list[bool] = []







        class FakeStream:



            def close(self) -> None:



                closed.append(True)







        stream = FakeStream()



        stream_registry.register_stream(stream)



        try:



            self.assertEqual(stream_registry.active_stream_count(), 1)



            self.assertEqual(stream_registry.close_active_streams(), 1)



            self.assertEqual(closed, [True])



            self.assertEqual(stream_registry.active_stream_count(), 0)



        finally:



            stream_registry.unregister_stream(stream)







    def test_close_ignores_streams_without_close_method(self) -> None:



        stream_registry.register_stream(object())



        self.assertEqual(stream_registry.close_active_streams(), 0)



        self.assertEqual(stream_registry.active_stream_count(), 0)







    def test_worker_registration_inherits_stream_scope(self) -> None:
        """并行工具线程注册的资源必须继承父回合归属。"""

        owner = object()
        resource = object()
        with stream_registry.stream_scope(owner):
            context = copy_context()
            with ThreadPoolExecutor(max_workers=1) as executor:
                executor.submit(context.run, stream_registry.register_stream, resource).result()

        try:
            self.assertEqual(stream_registry.active_stream_count(owner=owner), 1)
        finally:
            stream_registry.close_active_streams(owner=owner)

    def test_unregister_removes_stream(self) -> None:



        class FakeStream:



            def close(self) -> None:



                pass







        stream = FakeStream()



        stream_registry.register_stream(stream)



        stream_registry.unregister_stream(stream)



        self.assertEqual(stream_registry.active_stream_count(), 0)











if __name__ == "__main__":



    unittest.main()

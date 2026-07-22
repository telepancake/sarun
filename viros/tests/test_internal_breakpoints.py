import unittest

from inferiors.internal_breakpoints import (
    InternalBreakpoint,
    InternalBreakpointController,
    InternalBreakpointError,
    InternalBreakpointState,
)


class BackendFailure(RuntimeError):
    pass


class FakeBackend:
    def __init__(self):
        self.calls = []
        self.active = set()
        self.failures = {}

    def fail_next(self, operation, error="configured failure"):
        self.failures.setdefault(operation, []).append(BackendFailure(error))

    def _call(self, operation, *values):
        self.calls.append((operation, *values))
        failures = self.failures.get(operation, [])
        if failures:
            raise failures.pop(0)

    def insert_breakpoint(self, kind, address, size):
        self._call("insert", kind, address, size)
        self.active.add((kind, address, size))

    def remove_breakpoint(self, kind, address, size):
        self._call("remove", kind, address, size)
        self.active.remove((kind, address, size))

    def step_thread(self, thread_id):
        self._call("step", thread_id)

    def resume(self):
        self._call("resume")


class InternalBreakpointControllerTests(unittest.TestCase):
    def setUp(self):
        self.backend = FakeBackend()
        self.first = InternalBreakpoint(0x80100100, 4)
        self.second = InternalBreakpoint(0x80100200, 4)
        self.controller = InternalBreakpointController(
            self.backend, (self.first, self.second)
        )

    def test_steps_over_only_matching_internal_address_and_hides_step_stop(self):
        user_breakpoint = (1, 0x401000, 4)
        self.backend.active.add(user_breakpoint)
        self.controller.install()

        self.assertTrue(self.controller.owns_address(self.first.address))
        self.assertFalse(self.controller.owns_address(user_breakpoint[1]))
        self.assertFalse(self.controller.note_stop("2", user_breakpoint[1]))
        self.assertTrue(self.controller.note_stop("2", self.first.address))
        self.assertTrue(self.controller.begin_continue())

        self.assertNotIn((1, self.first.address, 4), self.backend.active)
        self.assertIn((1, self.second.address, 4), self.backend.active)
        self.assertIn(user_breakpoint, self.backend.active)
        self.assertEqual(self.backend.calls[-2:], [
            ("remove", 1, self.first.address, 4),
            ("step", "2"),
        ])

        self.assertTrue(self.controller.finish_step("2", 5))
        self.assertEqual(self.backend.calls[-2:], [
            ("insert", 1, self.first.address, 4),
            ("resume",),
        ])
        self.assertEqual(self.controller.state, InternalBreakpointState.READY)
        self.assertIsNone(self.controller.stop)
        self.assertIn(user_breakpoint, self.backend.active)

    def test_non_internal_and_non_plain_trap_stops_are_never_claimed(self):
        self.controller.install()
        self.assertFalse(self.controller.note_stop("1", 0x1234))
        self.assertFalse(
            self.controller.note_stop("1", self.first.address, signal=2)
        )
        self.assertFalse(
            self.controller.note_stop("1", self.first.address, watchpoint=True)
        )
        self.assertEqual(self.controller.state, InternalBreakpointState.READY)
        self.assertFalse(self.controller.begin_continue())

    def test_unexpected_step_stop_is_restored_but_reported_normally(self):
        self.controller.install()
        self.controller.note_stop("p1.2", self.second.address)
        self.controller.begin_continue()

        self.assertFalse(self.controller.finish_step("p1.2", 2))
        self.assertIn((1, self.second.address, 4), self.backend.active)
        self.assertNotIn(("resume",), self.backend.calls)
        self.assertEqual(self.controller.state, InternalBreakpointState.READY)

    def test_stop_on_a_different_thread_is_not_hidden(self):
        self.controller.install()
        self.controller.note_stop("1", self.first.address)
        self.controller.begin_continue()

        self.assertFalse(self.controller.finish_step("2", 5))
        self.assertIn((1, self.first.address, 4), self.backend.active)
        self.assertEqual(self.controller.state, InternalBreakpointState.READY)

    def test_step_failure_restores_breakpoint_and_preserves_failure(self):
        self.controller.install()
        self.controller.note_stop("1", self.first.address)
        self.backend.fail_next("step", "step unavailable")

        with self.assertRaises(InternalBreakpointError) as caught:
            self.controller.begin_continue()
        self.assertEqual(
            caught.exception.failure.operation, "single-step stopped QEMU thread"
        )
        self.assertEqual(self.controller.state, InternalBreakpointState.FAILED)
        self.assertIs(self.controller.failure, caught.exception.failure)
        self.assertIn((1, self.first.address, 4), self.backend.active)
        with self.assertRaises(InternalBreakpointError):
            self.controller.begin_continue()

    def test_remove_failure_does_not_step_and_preserves_stop_record(self):
        self.controller.install()
        self.controller.note_stop("1", self.first.address)
        self.backend.fail_next("remove", "remove unavailable")

        with self.assertRaises(InternalBreakpointError) as caught:
            self.controller.begin_continue()
        self.assertEqual(
            caught.exception.failure.operation,
            "remove internal breakpoint before step",
        )
        self.assertEqual(self.controller.state, InternalBreakpointState.FAILED)
        self.assertEqual(self.controller.stop.thread_id, "1")
        self.assertNotIn(("step", "1"), self.backend.calls)

    def test_restore_failure_after_step_never_resumes(self):
        self.controller.install()
        self.controller.note_stop("1", self.first.address)
        self.controller.begin_continue()
        self.backend.fail_next("insert", "restore unavailable")

        with self.assertRaises(InternalBreakpointError) as caught:
            self.controller.finish_step("1", 5)
        self.assertEqual(
            caught.exception.failure.operation,
            "restore internal breakpoint after step",
        )
        self.assertEqual(self.controller.state, InternalBreakpointState.FAILED)
        self.assertNotIn(("resume",), self.backend.calls)
        self.assertNotIn((1, self.first.address, 4), self.backend.active)

    def test_resume_failure_is_explicit_after_successful_restoration(self):
        self.controller.install()
        self.controller.note_stop("1", self.first.address)
        self.controller.begin_continue()
        self.backend.fail_next("resume", "resume unavailable")

        with self.assertRaises(InternalBreakpointError) as caught:
            self.controller.finish_step("1", 5)
        self.assertEqual(
            caught.exception.failure.operation,
            "resume after internal breakpoint step",
        )
        self.assertIn((1, self.first.address, 4), self.backend.active)
        self.assertEqual(self.controller.state, InternalBreakpointState.FAILED)

    def test_partial_install_failure_attempts_cleanup_and_records_both_failures(self):
        self.backend.fail_next("insert", "second insert unavailable")
        # Consume the configured failure on the second insertion only.
        original_insert = self.backend.insert_breakpoint
        insert_count = 0

        def insert_on_second(kind, address, size):
            nonlocal insert_count
            insert_count += 1
            if insert_count == 1:
                failures = self.backend.failures.pop("insert")
                try:
                    original_insert(kind, address, size)
                finally:
                    self.backend.failures["insert"] = failures
            else:
                original_insert(kind, address, size)

        self.backend.insert_breakpoint = insert_on_second
        self.backend.fail_next("remove", "cleanup unavailable")

        with self.assertRaises(InternalBreakpointError) as caught:
            self.controller.install()
        self.assertEqual(
            caught.exception.failure.operation, "install internal breakpoints"
        )
        self.assertEqual(len(caught.exception.failure.restoration), 1)
        self.assertEqual(self.controller.state, InternalBreakpointState.FAILED)
        self.assertEqual(self.controller.installed_breakpoints, (self.first,))

    def test_uninstall_removes_only_controller_owned_breakpoints(self):
        user_breakpoint = (1, 0x401000, 4)
        self.backend.active.add(user_breakpoint)
        self.controller.install()
        self.controller.uninstall()

        self.assertEqual(self.controller.state, InternalBreakpointState.CLOSED)
        self.assertEqual(self.controller.installed_breakpoints, ())
        self.assertEqual(self.backend.active, {user_breakpoint})


if __name__ == "__main__":
    unittest.main()

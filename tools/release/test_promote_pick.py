"""python3 -m unittest discover -s tools/release"""

import datetime
import unittest

from promote_pick import pick

NOW = datetime.datetime(2026, 11, 30, 12, 0, tzinfo=datetime.timezone.utc)


def rel(version, counter, days_ago, dev=True):
    return {"tag": f"v{version}-r{counter}", "version": version, "counter": counter, "dev": dev, "published": NOW - datetime.timedelta(days=days_ago)}


RELEASES = [
    rel("0.8.6", 76, 40, dev=False),
    rel("0.9.0-dev.1", 131, 15),
    rel("0.9.0-dev.2", 133, 8),
]


def no_notes(_tag):
    return ""


class Pick(unittest.TestCase):
    def test_the_newest_dev_release_after_seven_days(self):
        chosen, _ = pick(RELEASES, no_notes, NOW, 7)
        self.assertEqual(chosen, {"tag": "v0.9.0-dev.2-r133", "dev_version": "0.9.0-dev.2", "version": "0.9.0", "counter": 133, "dev_versions": "0.9.0-dev.1 0.9.0-dev.2"})

    def test_a_newer_dev_release_starts_the_wait_again(self):
        chosen, why = pick(RELEASES + [rel("0.9.0-dev.3", 136, 2)], no_notes, NOW, 7)
        self.assertIsNone(chosen)
        self.assertIn("v0.9.0-dev.3-r136 has been out for 2 days: stable on 2026-12-05 12:00 UTC", why)

    def test_held_by_a_line_in_its_notes(self):
        chosen, why = pick(RELEASES, lambda tag: "Fixes.\n\nhold\n", NOW, 7)
        self.assertIsNone(chosen)
        self.assertIn("held", why)
        # a word in a sentence is not a hold
        chosen, _ = pick(RELEASES, lambda tag: "Don't hold back.", NOW, 7)
        self.assertIsNotNone(chosen)

    def test_once_promoted_nothing_more(self):
        chosen, why = pick(RELEASES + [rel("0.9.0", 133, 1, dev=False)], no_notes, NOW, 7)
        self.assertIsNone(chosen)
        self.assertIn("stable release #133 is out already", why)

    def test_a_stable_name_is_not_used_twice(self):
        releases = [rel("0.9.0", 133, 20, dev=False), rel("0.9.0-dev.3", 136, 9)]
        chosen, why = pick(releases, no_notes, NOW, 7)
        self.assertIsNone(chosen)
        self.assertIn("0.9.1-dev.1", why)
        chosen, _ = pick(releases, no_notes, NOW, 7, version="0.9.0.1")
        self.assertEqual((chosen["version"], chosen["dev_versions"]), ("0.9.0.1", "0.9.0-dev.3"))

    def test_by_hand_any_dev_release_sooner_and_held(self):
        chosen, why = pick(RELEASES, no_notes, NOW, 7, tag="v0.9.0-dev.1-r131")
        self.assertEqual((chosen["version"], chosen["dev_versions"]), ("0.9.0", "0.9.0-dev.1"))
        young = RELEASES + [rel("0.9.0-dev.3", 136, 2)]
        self.assertIsNone(pick(young, no_notes, NOW, 7, tag="v0.9.0-dev.3-r136")[0])
        chosen, _ = pick(young, lambda tag: "hold", NOW, 7, tag="v0.9.0-dev.3-r136", force=True)
        self.assertEqual(chosen["tag"], "v0.9.0-dev.3-r136")
        self.assertIsNone(pick(RELEASES, no_notes, NOW, 7, tag="v0.8.6-r76")[0], "not a dev release")


if __name__ == "__main__":
    unittest.main()

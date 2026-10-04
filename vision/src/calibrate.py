# ==============================================================================
# Copyright (C) 2026 Guilherme Pedroza
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU Affero General Public License as
# published by the Free Software Foundation, either version 3 of the
# License, or (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU Affero General Public License for more details.
#
# You should have received a copy of the GNU Affero General Public License
# along with this program.  If not, see <https://www.gnu.org/licenses/>.
# ==============================================================================

import json

import os
import sys

os.environ["QT_LOGGING_RULES"] = "*=false"

import cv2

WINDOW_NAME = "Calibrate Parking Spots"


def extract_first_name(video_path: str):
    cap = cv2.VideoCapture(video_path)
    ret, frame = cap.read()
    cap.release()

    if not ret:
        print(f"Error: could not read {video_path}")
        sys.exit(1)

    return frame


class Calibrator:
    def __init__(self, frame):
        self.original = frame.copy()
        self.canvas = frame.copy()
        self.current_points: list = []
        self.spots: dict = {}
        self.spot_counter = 1

    def on_click(self, event, x, y, _flags, _params):
        if event != cv2.EVENT_LBUTTONDOWN:
            return

        self.current_points.append([x, y])
        cv2.circle(self.canvas, (x, y), 4, (0, 0, 255), -1)

        if len(self.current_points) > 1:
            pt1 = tuple(self.current_points[-2])
            pt2 = tuple(self.current_points[-1])
            cv2.line(self.canvas, pt1, pt2, (0, 255, 0), 2)

        cv2.imshow(WINDOW_NAME, self.canvas)

    def save_spot(self):
        if len(self.current_points) < 3:
            print("Need at least 3 points to define a parking spot.")
            return

        pt1 = tuple(self.current_points[-1])
        pt2 = tuple(self.current_points[0])
        cv2.line(self.canvas, pt1, pt2, (0, 255, 0), 2)

        spot_id = f"A-{self.spot_counter:02d}"
        self.spots[spot_id] = self.current_points.copy()
        self.current_points.clear()
        self.spot_counter += 1

        print(f"Saved {spot_id} ({len(self.spots[spot_id])} points)")
        cv2.imshow(WINDOW_NAME, self.canvas)

    def undo_last_point(self):
        if not self.current_points:
            return

        self.current_points.pop()

        self.canvas = self.original.copy()
        self._redraw_saved_spots()

        for i, pt in enumerate(self.current_points):
            cv2.circle(self.canvas, tuple(pt), 4, (0, 0, 255), -1)

            if i > 0:
                cv2.line(
                    self.canvas,
                    tuple(self.current_points[i - 1]),
                    tuple(pt),
                    (0, 255, 0),
                    2,
                )

        cv2.imshow(WINDOW_NAME, self.canvas)
        print("Undo last point.")

    def _redraw_saved_spots(self):
        for spot_id, points in self.spots.items():
            for i, pt in enumerate(points):
                cv2.circle(self.canvas, tuple(pt), 4, (0, 0, 255), -1)

                if i > 0:
                    cv2.line(
                        self.canvas,
                        tuple(points[i - 1]),
                        tuple(pt),
                        (0, 255, 0),
                        2,
                    )
                cv2.line(
                    self.canvas,
                    tuple(points[-1]),
                    tuple(points[0]),
                    (0, 255, 0),
                    2,
                )

    def export(self, output_path: str):
        with open(output_path, "w") as f:
            json.dump(self.spots, f, indent=2)
        print(f"Exported {len(self.spots)} spots to {output_path}")


def main():
    video_path = (
        sys.argv[1] if len(sys.argv) > 1 else os.environ.get("VISION_STREAM_URL")
    )
    output_path = (
        sys.argv[2] if len(sys.argv) > 2 else os.environ.get("VISION_SPOTS_PATH")
    )
    if not video_path or not output_path:
        raise SystemExit("Usage: python src/calibrate.py <video> <output-spots.json>")

    frame = extract_first_name(video_path)
    cal = Calibrator(frame)

    cv2.namedWindow(WINDOW_NAME, cv2.WINDOW_NORMAL)
    cv2.resizeWindow(WINDOW_NAME, 1280, 720)
    cv2.imshow(WINDOW_NAME, cal.canvas)
    cv2.setMouseCallback(WINDOW_NAME, cal.on_click)

    print("Controls:")
    print("     Click         - add point to current spot")
    print("     N             - finish current spot, start next")
    print("     U             - undo last point")
    print("     S             - save all spots and exit")
    print("     ESC           - exit without saving")

    while True:
        key = cv2.waitKey(0) & 0xFF

        if key == ord("n"):
            cal.save_spot()
        elif key == ord("u"):
            cal.undo_last_point()
        elif key == ord("s"):
            cal.export(output_path)
            break
        elif key == 27:
            print("Exiting without saving.")
            break

    cv2.destroyAllWindows()


if __name__ == "__main__":
    main()

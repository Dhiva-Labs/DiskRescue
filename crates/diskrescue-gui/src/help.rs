//! "Help": what to do, and not do, with a failing drive (from docs/USAGE.md,
//! in plain words).

use egui::{RichText, Ui};

use crate::theme::Kind;
use crate::widgets;

fn bullets(ui: &mut Ui, items: &[&str]) {
    for i in items {
        ui.horizontal_top(|ui| {
            ui.label("•");
            ui.add(egui::Label::new(*i).wrap());
        });
    }
}

fn situation(ui: &mut Ui, title: &str, body: &[&str]) {
    egui::CollapsingHeader::new(RichText::new(title).strong())
        .id_salt(title)
        .show(ui, |ui| {
            for p in body {
                ui.add(egui::Label::new(*p).wrap());
            }
        });
}

/// Draw the page.
pub fn show(ui: &mut Ui) {
    widgets::page_title(
        ui,
        "Help",
        "What to do, and what not to do, when photos and videos are missing.",
    );

    widgets::callout(ui, Kind::Warn, "First: stop using the drive", |ui| {
        bullets(
            ui,
            &[
                "Do not take more photos or videos with the card, and do not copy anything onto the \
                 drive. Every new file can overwrite deleted ones.",
                "Do not run \"Repair disk\", chkdsk or fsck, and do not format, even if the computer \
                 or camera offers to. These rewrite exactly what recovery needs.",
                "Save what you recover to a different disk, with enough free space.",
            ],
        );
    });
    ui.add_space(6.0);

    widgets::card(ui, |ui| {
        widgets::section(ui, "How DiskRescue works");
        ui.label(RichText::new("1. A careful copy of the drive").strong());
        ui.add(
            egui::Label::new(
                "DiskRescue reads the drive once into an image file: the healthy parts first, then \
                 it comes back for the damaged ones. This is the only step that touches the drive, \
                 and it can be stopped and continued later. For a failing drive, always copy first.",
            )
            .wrap(),
        );
        ui.label(RichText::new("2. Finding your files in the copy").strong());
        ui.add(
            egui::Label::new(
                "It finds photos and videos by name (including deleted ones), then searches the raw \
                 data for files whose names are gone, checks every file, and repairs damaged ones \
                 when it can. Damaged originals are always kept next to the repaired copies.",
            )
            .wrap(),
        );
        ui.label(RichText::new("3. Looking at the results").strong());
        ui.add(
            egui::Label::new(
                "The Results page shows everything that came back. Look at deleted files first, then \
                 at files found in the raw data (their names are lost, so they are named by date).",
            )
            .wrap(),
        );
    });
    ui.add_space(6.0);

    widgets::card(ui, |ui| {
        widgets::section(ui, "Common situations");
        situation(
            ui,
            "The camera says \"card error\", or the computer asks to format the card",
            &[
                "The list of files on the card is damaged, but the photos themselves are usually \
                 fine. Do not format it.",
                "Rescue the card with \"Copy the drive to an image first\" turned on. Searching the \
                 raw data finds the photos even when the file list cannot be read.",
            ],
        );
        situation(
            ui,
            "The USB drive disconnects, freezes or is very slow",
            &[
                "Copy it to an image first. DiskRescue waits for the drive to come back when it \
                 drops off, and recognises it by its serial number.",
                "If it keeps disconnecting, try another USB port or cable. A drive that shows a size \
                 of 0 bytes or a few MB has a broken controller: software cannot fix that, but a data \
                 recovery lab often can.",
            ],
        );
        situation(
            ui,
            "The hard disk clicks, is very slow, or shows read errors",
            &[
                "Copy it once, carefully, and stop if the noise gets worse: every hour a failing \
                 disk runs can cost data. DiskRescue copies everything healthy first.",
                "Never open the disk. If it clicks all the time and reads nothing, switch it off and \
                 contact a data recovery lab.",
            ],
        );
        situation(
            ui,
            "Photos deleted from an SSD",
            &[
                "SSDs usually erase deleted files within minutes (this is called TRIM), so getting \
                 them back rarely works. Files that still exist on a failing SSD can be recovered: \
                 copy it to an image first, and do not switch it on and off more than needed.",
            ],
        );
        situation(
            ui,
            "Photos deleted by accident from a healthy card",
            &[
                "Stop using the card. Copying it first is still the safest choice; reading it \
                 directly is fine if it works normally.",
                "Look in the Deleted filter of the Results page first.",
            ],
        );
        situation(
            ui,
            "A video from a phone or action camera will not play",
            &[
                "Use \"Repair photos & videos\" and add a reference file: any video that does play, \
                 recorded with the same phone or camera at the same resolution and frame rate.",
            ],
        );
    });
    ui.add_space(6.0);

    widgets::card(ui, |ui| {
        widgets::section(ui, "When to stop and get help");
        bullets(
            ui,
            &[
                "The drive shows a size of 0 bytes, or much less than it should.",
                "A hard disk clicks continuously, beeps, or does not spin up.",
                "The drive smells burnt or got wet.",
            ],
        );
        ui.add(
            egui::Label::new(
                "In these cases software cannot help, and every attempt can make things worse. A \
                 data recovery lab can often still get the files back.",
            )
            .wrap(),
        );
    });
    ui.add_space(6.0);

    widgets::card(ui, |ui| {
        widgets::section(ui, "Administrator rights");
        ui.add(
            egui::Label::new(
                "Reading a drive directly needs administrator rights. DiskRescue asks for your \
                 password only to copy the drive; finding and repairing files runs with your normal \
                 rights, and the recovered files belong to you. On Windows, start DiskRescue with \
                 \"Run as administrator\".",
            )
            .wrap(),
        );
        ui.add(
            egui::Label::new(
                "DiskRescue never writes to the drive being rescued, and refuses to save anything \
                 onto it.",
            )
            .wrap(),
        );
    });
}

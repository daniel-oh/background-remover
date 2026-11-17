//! Which model, and what it expects.
//!
//! Matting models of this kind all take a `[1, 3, 1024, 1024]` float picture
//! and give back one plane, but they differ in how the picture is scaled on
//! the way in and in whether the plane still needs a sigmoid on the way
//! out. A [`Profile`] is those differences; [`KNOWN`] is the models this
//! program can name, fetch and check by itself.

/// How a model wants its input sized and scaled, and its output read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Profile {
    /// The square the picture is resized to on the way in, and the size of
    /// the mask that comes out.
    pub side: u32,
    /// Subtracted from each channel after dividing by 255.
    pub mean: [f32; 3],
    /// Each channel is then divided by this.
    pub std: [f32; 3],
    /// The output is logits: put it through a sigmoid before rescaling.
    pub sigmoid: bool,
}

impl Profile {
    /// The DIS / isnet family, as rembg's `DisSession` feeds it.
    pub const ISNET: Profile = Profile {
        side: 1024,
        mean: [0.5; 3],
        std: [1.0; 3],
        sigmoid: false,
    };

    /// BiRefNet: ImageNet statistics in, logits out.
    pub const BIREFNET: Profile = Profile {
        side: 1024,
        mean: [0.485, 0.456, 0.406],
        std: [0.229, 0.224, 0.225],
        sigmoid: true,
    };

    /// The same scaling at another input size.
    pub const fn at(self, side: u32) -> Profile {
        Profile { side, ..self }
    }

    /// A profile by name (`isnet` or `birefnet`), for a model file this
    /// program does not know.
    pub fn named(name: &str) -> Option<Profile> {
        match name {
            "isnet" => Some(Profile::ISNET),
            "birefnet" => Some(Profile::BIREFNET),
            _ => None,
        }
    }
}

/// A model this program knows how to fetch, check and run.
#[derive(Debug)]
pub struct Known {
    /// What to call it: `MODEL`, or `--model`.
    pub name: &'static str,
    /// Its file name in the cache directory.
    pub file: &'static str,
    /// Where its weights are published.
    pub url: &'static str,
    /// What the file must hash to.
    pub sha256: &'static str,
    /// Size of the download, for the progress line.
    pub megabytes: u32,
    /// How to feed it and read it.
    pub profile: Profile,
}

/// The model used when none is named.
pub const DEFAULT: &str = "isnet-general-use";

/// The models that can be named. The first is the default: quick and
/// light. The BiRefNet ones (MIT, Zheng et al., 2024) cut out people,
/// hair, thin structures and cluttered scenes visibly better; the README
/// has the comparison and the measurements.
///
/// `birefnet-lite-768` is BiRefNet_lite exported by this project at 768 by
/// 768 (`scripts/export-birefnet.py`) and published with its releases: at
/// that size it keeps nearly all of the 1024 model's quality for about a
/// third of the time and memory, which is what makes it fit a small server.
pub const KNOWN: [Known; 4] = [
    Known {
        name: "isnet-general-use",
        file: "isnet-general-use.onnx",
        url: "https://github.com/danielgatis/rembg/releases/download/v0.0.0/isnet-general-use.onnx",
        sha256: "60920e99c45464f2ba57bee2ad08c919a52bbf852739e96947fbb4358c0d964a",
        megabytes: 178,
        profile: Profile::ISNET,
    },
    Known {
        name: "birefnet-lite-768",
        file: "birefnet-lite-768.onnx",
        url: "https://github.com/daniel-oh/background-remover/releases/download/models-1/birefnet-lite-768.onnx",
        sha256: "16eedf6a83dc37dfa021651cb856590e640050938486fbdfc96404c2ed74418c",
        megabytes: 187,
        profile: Profile::BIREFNET.at(768),
    },
    Known {
        name: "birefnet-general-lite",
        file: "birefnet-general-lite.onnx",
        url: "https://github.com/ZhengPeng7/BiRefNet/releases/download/v1/BiRefNet-general-bb_swin_v1_tiny-epoch_232.onnx",
        sha256: "5600024376f572a557870a5eb0afb1e5961636bef4e1e22132025467d0f03333",
        megabytes: 214,
        profile: Profile::BIREFNET,
    },
    Known {
        name: "birefnet-general",
        file: "birefnet-general.onnx",
        url: "https://github.com/ZhengPeng7/BiRefNet/releases/download/v1/BiRefNet-general-epoch_244.onnx",
        sha256: "58f621f00f5d756097615970a88a791584600dcf7c45b18a0a6267535a1ebd3c",
        megabytes: 928,
        profile: Profile::BIREFNET,
    },
];

/// The known model of that name.
pub fn known(name: &str) -> Option<&'static Known> {
    KNOWN.iter().find(|k| k.name == name)
}

/// The names that can be given, for an error message.
pub fn names() -> String {
    KNOWN.map(|k| k.name).join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_known_and_first() {
        assert_eq!(known(DEFAULT).unwrap().name, KNOWN[0].name);
        assert_eq!(known(DEFAULT).unwrap().profile, Profile::ISNET);
        assert!(known("no-such-model").is_none());
    }

    #[test]
    fn every_known_model_is_whole() {
        for k in &KNOWN {
            assert_eq!(k.sha256.len(), 64, "{}", k.name);
            assert!(
                k.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "{}",
                k.name
            );
            assert!(k.url.starts_with("https://"), "{}", k.name);
            assert!(k.file.ends_with(".onnx"), "{}", k.name);
            assert!(k.profile.std.iter().all(|&s| s > 0.0), "{}", k.name);
            assert!(k.profile.side % 32 == 0, "{}", k.name);
        }
        let mut files: Vec<_> = KNOWN.iter().map(|k| k.file).collect();
        files.dedup();
        assert_eq!(files.len(), KNOWN.len(), "no two models share a file");
    }

    #[test]
    fn profiles_have_names() {
        assert_eq!(Profile::named("isnet"), Some(Profile::ISNET));
        assert_eq!(Profile::named("birefnet"), Some(Profile::BIREFNET));
        assert_eq!(Profile::named("resnet"), None);
    }
}

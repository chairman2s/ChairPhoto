-- ChairPhoto is colour-critical: the default translucency mixes the wallpaper into
-- every tone the user is judging in Develop. Kept fully opaque, like DaVinci Resolve.
-- The class match also covers the Loupe window, which shares the class.
o.window("^chairphoto$", { tag = "-default-opacity", opacity = "1 1" })

package com.kairo.vistrel;

import java.util.Locale;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

public class Constants {
    public static final String MOD_ID = "vistrel";
    public static final String MOD_NAME = "Vistrel";
    public static final Logger LOG = LoggerFactory.getLogger(MOD_NAME);
    private static final String OS_NAME = System.getProperty("os.name").toLowerCase(Locale.ROOT);
    public static final String KEY_NATIVE_LIB = "vistrel.nativeLib";
    public static final String NATIVE_LIB = System.getProperty(KEY_NATIVE_LIB);
    public static final boolean IS_WINDOWS = OS_NAME.contains("win");
    public static final boolean IS_LINUX = OS_NAME.contains("linux");
}
